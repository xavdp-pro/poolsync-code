# Remote desktop clipboard compatibility

With `pause_clipboard_when_rdp = true`, a detected clipboard-enabled FreeRDP
client owns its local clipboard transport. PoolSync stops ordinary local
clipboard publication and application there, but still receives and relays
shared history. The remote graphical session needs its own participating agent
to distribute copies through the pool. This pause is not a privacy departure.

The current detector handles `xfreerdp` / `xfreerdp3` with a destination argument.
It excludes `-clipboard` and `/clipboard:direction-to:off`. Remmina and other
clients are not universally detected. A running process does not prove that the
clipboard channel negotiated successfully. Several native clients can interact
with one clipboard; PoolSync does not choose between their remote sessions.

Keep the chosen policy consistent and qualify it with real GTK and browser
pastes on both client and server. A BMP image that pastes in GTK may be empty in
a browser. FreeRDP generic PNG support and WinPR clipboard PNG conversion are
separate: `WINPR_UTILS_IMAGE_PNG=ON` enables the latter. Verify the actual loaded
library and native paste rather than relying only on CLI build flags.

Native XRDP ownership can temporarily interrupt browser image paste even when
another receiver gets the expected pixels. Treat an empty paste as a failure;
do not claim a repair from a subsequent successful retry. Qualify any dependency
repair independently before changing a desktop runtime.

Closing a client normally can leave the remote graphical session logged in.
The remote agent still depends on that graphical session. Verify credentials
and a restoration path before unattended disconnect/reconnect tests. These are
compatibility constraints, not a reason to require a permanent PoolSync hub.
