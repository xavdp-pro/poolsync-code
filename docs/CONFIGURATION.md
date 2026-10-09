# Node and layout configuration

`examples/agent.desk-a.toml` is a fictitious template, not a usable identity.
Replace every `REPLACE_...` field and path privately. All root settings precede
TOML tables. The parser still requires legacy `hub_url` and `token` fields;
`hubless = true` bypasses the hub session, and `hub_clipboard = false` disables
hub clipboard forwarding. An unreachable compatibility URL does not create a
server requirement.

Choose a distinct node name and `node_token` for each computer. `[peer_tokens]`
maps a sending peer's name to that peer's own token. Use one intended shared
32-byte base64 encryption key and trusted node certificates. Every peer URL must
be reachable and match its certificate SAN. Review reciprocal authorization;
unknown devices do not enroll automatically.

For KVM use `mode = "full"`, `kvm_enabled = true`, `kvm_capture = true`. For
clipboard-only use `mode = "clipboard_only"` and both flags false. Preserve the
chosen `pause_clipboard_when_rdp` policy. A session's role does not automatically
apply to every graphical session hosted by that computer.

Saved layout is `agent.topology.json`. For a new pool only, use the example as a
starting point and adapt it to the real virtual desktop bounds. Configure one
consistent document, exclude clipboard-only nodes from KVM and review adjacency
before accepting screen-edge control. Preserve revision, origin and positions
on existing pools. Network routes are not physical screen positions.

To add a node, enroll its identity, update reciprocal peer routes/authorization
and layout, then qualify the enlarged cohort. To temporarily leave, use the tray
or `poolsync-agent --config PATH --away true`; return with `--away false`. For
permanent retirement, remove authorization/routes/layout coherently and review
the appropriate credential-rotation policy.
