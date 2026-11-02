//! `caucus ctl` — one control-socket call from the command line
//! (`docs/design.md` §10). It speaks the envelope `caucus mcp-serve` speaks
//! ([`crate::mcp::protocol`]): `method` is an MCP tool name and `params` its
//! arguments, decoded and executed by the main process exactly as a tool call
//! from the main worker would be.

use std::io::{BufRead, BufReader, Read, Write};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::mcp::protocol::ControlCall;

/// Send `call` over `stream` and copy every reply line to `out` until the
/// server closes the connection. Returns whether every line was a success.
pub fn exchange(
    mut stream: impl Read + Write,
    call: &ControlCall,
    out: &mut impl Write,
) -> Result<bool> {
    let mut line = serde_json::to_string(call).context("serialise control call")?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .context("write control call")?;
    stream.flush().context("flush control call")?;

    let mut ok = true;
    for reply in BufReader::new(stream).lines() {
        let reply = reply.context("read control reply")?;
        writeln!(out, "{reply}").context("print control reply")?;
        ok &= serde_json::from_str::<Value>(&reply).is_ok_and(|v| v.get("error").is_none());
    }
    Ok(ok)
}
