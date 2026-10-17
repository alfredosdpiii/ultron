//! The built-in MCP gateway next to pi-mcp-adapter (extensions/mcp/coexistence.ts): the adapter registered an `mcp`
//! gateway and `/mcp` like the built-in one, so it is left out when the built-in gateway is loaded (MCP is native
//! now, and its `mcp.json` keeps working), and the session says so once when it starts. Here the adapter, a
//! JavaScript extension, would not load anyway: instead of the "no longer loaded" diagnostic it gets this notice.

use std::path::Path;

/// `ADAPTER_NOTICE`.
pub const ADAPTER_NOTICE: &str = "pi-mcp-adapter is installed but no longer needed: Ultron connects MCP servers itself and reads the same mcp.json. The adapter was not loaded; remove it with `ultron remove npm:pi-mcp-adapter`.";

/// `ADAPTER_PACKAGE` (`(^|[\\/])pi-mcp-adapter([\\/]|$)`): a path with a `pi-mcp-adapter` component.
pub fn is_mcp_adapter(path: &Path) -> bool {
    path.to_string_lossy()
        .split(['/', '\\'])
        .any(|part| part == "pi-mcp-adapter")
}
