use super::*;
use crate::mcp::{McpError, McpToolSurface, PanelSummary, ReadPanelMode};
use crate::role::spec::AgentCli;
use crate::session::id::PanelId;
use crate::worktree::cleanup::CleanupJob;

/// The main worker's MCP tool surface, backed by the live panel registry
/// (`docs/design.md` §0 #4). Every method runs on the multiplexer's own
/// thread — control jobs are executed by [`Multiplexer::drain_control`] inside
/// the event loop, never concurrently with `pump_all` (Invariant I-5).
impl McpToolSurface for Multiplexer {
    fn send_keys(&mut self, panel: PanelId, text: &str, enter: bool) -> Result<(), McpError> {
        let p = self
            .panels
            .iter_mut()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        let mut bytes = text.as_bytes().to_vec();
        if enter {
            bytes.push(b'\r');
        }
        p.write_input(&bytes)
            .map_err(|e| McpError::Tool(format!("send_keys: {e}")))?;

        // Delivering a prompt opens a capture turn and flips the panel to
        // `Working` (`docs/design.md` §4) — only when the line is submitted,
        // since a partial line is not yet a turn.
        if enter {
            self.note_prompt_delivered(panel);
        }
        Ok(())
    }

    fn ctrl_c(&mut self, panel: PanelId) -> Result<(), McpError> {
        let p = self
            .panels
            .iter_mut()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        // 0x03 = ETX = Ctrl-C.
        p.write_input(&[0x03])
            .map_err(|e| McpError::Tool(format!("ctrl_c: {e}")))
    }

    fn read_panel(&self, panel: PanelId, mode: ReadPanelMode) -> Result<String, McpError> {
        let p = self
            .panels
            .iter()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        Ok(match mode {
            ReadPanelMode::Screen => Self::screen_text(p),
            ReadPanelMode::Scrollback => Self::scrollback_text(p),
            ReadPanelMode::SinceLastTurn => {
                // Whole-turn capture (`docs/design.md` §8.5), rendered to
                // readable text — the main worker never races the screen and
                // is never handed raw escape sequences.
                let (cols, _) = p.grid().size();
                Self::rendered_capture_text(p.capture().since_last_turn(), cols)
            }
            ReadPanelMode::LastMessage => self
                .manifests
                .get(&panel)
                .and_then(|m| m.last_message().map(str::to_string))
                .unwrap_or_default(),
        })
    }

    fn spawn_role(
        &mut self,
        role: &str,
        worktree: bool,
        model: Option<&str>,
        agent_cli: Option<AgentCli>,
    ) -> Result<PanelId, McpError> {
        let wt_handle = if worktree {
            Some(self.create_role_worktree(role)?)
        } else {
            None
        };
        let worktree_path = wt_handle.as_ref().map(|h| h.path.clone());
        let worktree_branch = wt_handle.as_ref().map(|h| h.branch.clone());
        // `spawn_panel_resume` with no resume id is a plain spawn that also
        // records the worktree branch (so `caucus resume` can re-attach it).
        let spawned = self.spawn_panel_resume(
            role,
            agent_cli,
            model.map(str::to_string),
            worktree_path,
            worktree_branch,
            None,
        );
        match spawned {
            Ok(id) => {
                self.persist_record();
                Ok(id)
            }
            Err(e) => {
                // The panel never came up — don't leak the worktree (dir +
                // branch) `create_role_worktree` just created. Enqueue it for
                // serial cleanup (Invariant I-3); the branch is empty (the
                // sub-agent never ran) so it is deleted, not preserved.
                if let Some(h) = wt_handle {
                    let _ = self.cleanup.enqueue(CleanupJob {
                        repo_root: self.session.repo_path.clone(),
                        worktree_paths: vec![h.path],
                        branches_to_delete: vec![h.branch],
                        done: None,
                    });
                }
                Err(McpError::Tool(format!("spawn_role: {e:#}")))
            }
        }
    }

    fn kill_panel(&mut self, panel: PanelId) -> Result<(), McpError> {
        if !self.panels.iter().any(|p| p.id == panel) {
            return Err(McpError::NoSuchPanel(panel));
        }
        // Delegate to the inherent single-owner destruction path.
        Multiplexer::kill_panel(self, panel)
            .map_err(|e| McpError::Tool(format!("kill_panel: {e:#}")))
    }

    fn list_panels(&self) -> Vec<PanelSummary> {
        self.panels
            .iter()
            .map(|p| {
                // Prefer the manifest's derived_state (turn-signal fed); fall
                // back to the coarse panel-state label before the first turn.
                // A live selection menu on the grid overlays `awaiting_selection`
                // (no Stop hook fires while a chooser is open).
                let (state, agent_cli) = match self.manifests.get(&p.id) {
                    Some(m) => {
                        let st = Self::overlay_menu_state(
                            m.derived_state(),
                            Self::panel_menu(p).is_some(),
                        );
                        (st.as_str().to_string(), m.agent_cli)
                    }
                    None => (p.state_label().to_string(), AgentCli::Claude),
                };
                PanelSummary {
                    panel_id: p.id,
                    role: p.role.clone(),
                    state,
                    agent_cli,
                }
            })
            .collect()
    }

    fn read_menu(&self, panel: PanelId) -> Result<String, McpError> {
        let p = self
            .panels
            .iter()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        Ok(match Self::panel_menu(p) {
            Some(menu) => Self::render_menu(&menu),
            None => "(no selection menu visible on this panel)".to_string(),
        })
    }

    fn select_option(&mut self, panel: PanelId, index: usize) -> Result<(), McpError> {
        // Scan the menu (immutable) before writing (mutable) — no overlapping
        // borrows of `self.panels`.
        let menu = {
            let p = self
                .panels
                .iter()
                .find(|p| p.id == panel)
                .ok_or(McpError::NoSuchPanel(panel))?;
            Self::panel_menu(p)
                .ok_or_else(|| McpError::Tool("no selection menu visible on this panel".into()))?
        };
        let target = menu
            .options
            .iter()
            .position(|o| o.number == index)
            .ok_or_else(|| {
                McpError::Tool(format!(
                    "no option {index} in the menu (options 1..={})",
                    menu.options.len()
                ))
            })?;
        let bytes = Self::menu_nav_bytes(menu.cursor, target);

        let p = self
            .panels
            .iter_mut()
            .find(|p| p.id == panel)
            .ok_or(McpError::NoSuchPanel(panel))?;
        p.write_input(&bytes)
            .map_err(|e| McpError::Tool(format!("select_option: {e}")))?;
        // Submitting a selection resumes the agent's turn — open a capture turn
        // and flip the panel to `Working`, exactly like the `send_keys` path.
        self.note_prompt_delivered(panel);
        Ok(())
    }
}

impl Multiplexer {
    /// Render a detected [`crate::term::Menu`] as readable text for the main
    /// worker: the question, the numbered options (the highlighted one marked),
    /// and how to answer.
    pub(crate) fn render_menu(menu: &crate::term::Menu) -> String {
        let mut out = String::from("selection menu:\n");
        if !menu.question.is_empty() {
            out.push_str(&format!("question: {}\n", menu.question));
        }
        for (i, opt) in menu.options.iter().enumerate() {
            let marker = if i == menu.cursor { "❯ " } else { "  " };
            out.push_str(&format!("{marker}{}. {}\n", opt.number, opt.label));
        }
        out.push_str("(answer with select_option(panel, <number>))");
        out
    }

    /// Bytes that move a chooser's cursor from `cursor` to `target` and submit:
    /// `|target-cursor|` arrow keys (down when target is lower, up otherwise)
    /// then Enter. Reuses [`crate::input::encode_key`] so the xterm sequences
    /// match what a real keyboard would send.
    fn menu_nav_bytes(cursor: usize, target: usize) -> Vec<u8> {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (code, count) = if target >= cursor {
            (KeyCode::Down, target - cursor)
        } else {
            (KeyCode::Up, cursor - target)
        };
        let arrow =
            crate::input::encode_key(&KeyEvent::new(code, KeyModifiers::NONE)).unwrap_or_default();
        let mut bytes = Vec::new();
        for _ in 0..count {
            bytes.extend_from_slice(&arrow);
        }
        bytes.extend_from_slice(
            &crate::input::encode_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap_or_default(),
        );
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `menu_nav_bytes` emits the right count + direction of arrow keys, then
    /// Enter — per navigation boundary.
    #[test]
    fn menu_nav_bytes_moves_the_cursor_and_submits() {
        // Down two then Enter (cursor 0 → option index 2).
        assert_eq!(Multiplexer::menu_nav_bytes(0, 2), b"\x1b[B\x1b[B\r");
        // Up two then Enter (cursor 2 → index 0).
        assert_eq!(Multiplexer::menu_nav_bytes(2, 0), b"\x1b[A\x1b[A\r");
        // Already on target: just Enter, no arrows.
        assert_eq!(Multiplexer::menu_nav_bytes(1, 1), b"\r");
    }

    /// `render_menu` lists the options and marks the highlighted one.
    #[test]
    fn render_menu_marks_the_cursor_option() {
        let menu = crate::term::Menu {
            question: "Pick one".to_string(),
            options: vec![
                crate::term::MenuOption {
                    number: 1,
                    label: "alpha".to_string(),
                },
                crate::term::MenuOption {
                    number: 2,
                    label: "beta".to_string(),
                },
            ],
            cursor: 1,
        };
        let text = Multiplexer::render_menu(&menu);
        assert!(text.contains("question: Pick one"));
        assert!(text.contains("❯ 2. beta"), "cursor option marked: {text:?}");
        assert!(text.contains("  1. alpha"));
        assert!(text.contains("select_option"));
    }
}
