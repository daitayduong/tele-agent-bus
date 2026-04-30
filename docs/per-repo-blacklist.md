# Per-Repo Blacklists

The agent-bus supports per-repository blacklists, which allow for more granular control over command execution. This document explains how they work and how to use them.

## Merge Semantics (UNION)

Per-repo blacklists are merged with the global blacklist using UNION semantics. This means that a command is forbidden if it matches a pattern in *either* the global blacklist or the per-repo blacklist.

**Crucially, per-repo blacklists can only *add* restrictions; they can never relax rules defined in the global blacklist.**

For example:
- If `^rm -rf` is in the global blacklist...
- And the per-repo blacklist for `my-project` does *not* contain `^rm -rf`...
- The `rm -rf` command will still be blocked in `my-project` because of the global rule.

If a per-repo blacklist file is missing, it is treated as an empty list of patterns. If either the global or per-repo blacklist file is tampered with (i.e., the HMAC signature does not match), the system will fail-closed, denying all command executions.

## CLI Usage

The `agent-bus blacklist` subcommands (`add`, `remove`, `list`, `verify`) all accept a `--repo <repo_id>` flag to operate on a per-repo blacklist.

**Examples:**

*   **Add a rule:**
    ```bash
    agent-bus blacklist add --repo my-project '^git push --force'
    ```

*   **List rules:**
    ```bash
    agent-bus blacklist list --repo my-project
    ```

*   **Remove a rule:**
    ```bash
    agent-bus blacklist remove --repo my-project '^git push --force'
    ```

*   **Verify integrity:**
    ```bash
    agent-bus blacklist verify --repo my-project
    ```

When the `--repo` flag is used, these commands do **not** require `sudo`.

## Storage Location

Per-repo blacklists are stored in your agent-bus home directory, typically `~/.agent-bus/`. Each repository has a dedicated subdirectory named after its ID.

- **Path:** `~/.agent-bus/repos/<repo_id>/blacklist.conf`
- **HMAC:** `~/.agent-bus/repos/<repo_id>/blacklist.conf.hmac`

These files are owned by the user who runs the `agent-bus blacklist` commands.

## Key Sharing and Security

Per-repo blacklists are signed using the same shared HMAC key as the global blacklist.

- **Key Location:** `/etc/agent-bus/blacklist.key`

This key is owned by `root:agent-bus` with `0640` permissions. To read this key for per-repo operations, your user account must be a member of the `agent-bus` group.

**To add your user to the group:**

```bash
sudo usermod -aG agent-bus $USER
```

After running this command, you must **log out and log back in** for the group change to take effect.

The rationale for sharing the key is:
1.  **Simpler Key Management:** There is only one key to manage and rotate.
2.  **Limited Risk:** Since per-repo blacklists can only add new restrictions, the risk of a compromised user account tampering with a blacklist is limited to a denial-of-service (by adding overly restrictive rules), not a security breach (by removing rules). Write access to the key file itself is still protected by `sudo`.
