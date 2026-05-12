//! Fallback settle detection when the Claude `Stop` hook is not installed.
//! Regex-based pane-screen hints + a low-rate poller, modelled on dmux's
//! `PaneWorker` (see `docs/dmux-analysis.md` §2).
