# Use Fleet with new machines

1. Enable SSH on each target computer.
   - Mac: System Settings → General → Sharing → **Remote Login**. Allow your account.
   - Linux: install and enable your distribution's OpenSSH server.
   - Windows: install **OpenSSH Server** and start `sshd`.
2. Connect your Mac and targets through a reachable LAN or VPN. Allow the SSH port. Tailscale is optional.
3. In your Mac's Terminal, run `ssh user@host` for each target. Verify new host fingerprints through a trusted channel. Confirm you reach its shell.
4. Open Fleet.app on your Mac. If needed, [build it locally](../README.md#develop-and-build).
5. Click **Add host**. Enter a unique **Host label** and the working `user@host` or SSH alias in **SSH target**.
6. Leave overrides blank to keep your SSH settings. Click **Save host**. Repeat for every target.
7. Choose **Plain SSH…**, then **Open plain SSH**. If asked, verify the fingerprint before **Accept verified key**.
8. Optional on Mac or Linux: install `tmux` on the target, enter a session label, and choose **Create tmux**. Use **Attach** after reopening Fleet.

Plain SSH does not preserve processes after disconnect. tmux requires its original session and pane to stay alive. Reboots do not restore them. Fleet does not reconnect automatically.

NAS and the local Studio hub remain excluded from enrollment. Before building for your network, configure the [compile-time exclusions](../README.md#configure-local-exclusions). Keep real addresses, accounts, and host-key aliases in private local configuration, not these instructions.
