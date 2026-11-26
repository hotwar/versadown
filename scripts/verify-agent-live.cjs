// One isolated profile covers both CLI and MCP. The shared script owns all
// spawned processes, fixture downloads and JSON-RPC/CDP evidence.
// Usage: node scripts/verify-agent-live.cjs <results-directory> [quick-artifacts-directory]
require('./verify-cli-live.cjs');
