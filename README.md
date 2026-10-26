# switch-mcp

Lets an AI agent develop and test on a real Nintendo Switch (Atmosphère) without anyone holding it:
deploy builds, launch and force-close them, reboot, take screenshots, press buttons, read memory and
fetch logs and crash reports.

Two parts, both Rust:
- `daemon/`: a sysmodule that runs on the Switch and executes commands.
- `mcp/`: an MCP server on the PC that passes the agent's tool calls to the daemon.

Status: design. Nothing built yet. See `context/DESIGN.md`.
