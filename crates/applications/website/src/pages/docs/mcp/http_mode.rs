use crate::components::CodeBlock;
use leptos::*;

#[component]
pub fn McpHttpMode() -> impl IntoView {
    view! {
        <div>
            <h1>"HTTP Mode"</h1>
            <p class="lead">"Run the GFS MCP server over HTTP for daemon mode or remote access."</p>

            <h2>"Daemon Mode"</h2>
            <p>"Start the MCP server as a background daemon:"</p>
            <CodeBlock code="gfs mcp start"/>
            <p>"The server listens on "<code>"http://127.0.0.1:3000/mcp"</code>" by default. Manage the daemon with:"</p>
            <ul>
                <li><code>"gfs mcp status"</code>" - Check if the daemon is running"</li>
                <li><code>"gfs mcp stop"</code>" - Stop the daemon"</li>
                <li><code>"gfs mcp restart"</code>" - Restart the daemon"</li>
            </ul>

            <h2>"Foreground HTTP"</h2>
            <p>"Run the HTTP server in the foreground (useful for debugging):"</p>
            <CodeBlock code="gfs mcp web
# Or with custom port:
gfs mcp web --port 8080"/>

            <h2>"With Repository Path"</h2>
            <CodeBlock code="gfs mcp --path /path/to/repo start
    gfs mcp --path /path/to/repo web --port 8080"/>

            <h2>"Endpoint"</h2>
            <p>"Clients send JSON-RPC requests to "<code>"POST http://127.0.0.1:PORT/mcp"</code>". The server uses the streamable HTTP transport, and every request must carry "<code>"Authorization: Bearer <token>"</code>". Set "<code>"GFS_MCP_TOKEN"</code>", or read the token the server generates from "<code>"~/.gfs/mcp-token"</code>". The stdio transport needs no token."</p>

            <h2>"Use Cases"</h2>
            <ul>
                <li>"CI/CD pipelines - Call MCP tools from scripts"</li>
                <li>"Remote management - Access from another machine on the network"</li>
                <li>"Custom tooling - Build UIs or integrations on top of the HTTP API"</li>
            </ul>

            <h2>"See Also"</h2>
            <ul>
                <li><a href="/docs/mcp/overview">"MCP Overview"</a>" - Tools and architecture"</li>
                <li><a href="/docs/mcp/claude-desktop">"Claude Desktop"</a>" - Stdio integration"</li>
                <li><a href="/docs/mcp/claude-code">"Claude Code"</a>" - IDE extension"</li>
                <li><a href="/docs/mcp/cursor">"Cursor"</a>" - Cursor IDE"</li>
            </ul>
        </div>
    }
}
