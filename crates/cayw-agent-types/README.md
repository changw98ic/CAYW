# cayw-agent-types

Owns the cross-component `AgentEvent` and `ToolResult` contracts shared by the
agent loop and durable Core storage.

This is an implementation-boundary crate, not an independent wire or disk
format. Both types still use `cayw-ai` message, stream and usage types, and Core
continues to read and write Core v3 bytes. `cayw-agent` re-exports both types at
their original paths.

Do not add runtime errors, task or resource types, executors, model calls or
database operations here. Any change to these events must review agent
semantics, Core read/write compatibility and real history data together.
