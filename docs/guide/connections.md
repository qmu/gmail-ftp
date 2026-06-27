# Connections & credentials

You don't need any credential to **describe** a path or **preview** a query — both are completely
offline. You only need one to **commit** against a live service.

## Storing a credential

```sh
qfs connection add <service> <name>
```

- `<service>` is the driver the connection belongs to — `mail`, `s3`, `github`, `slack`, `sql`, …
- `<name>` is your label for it — `work`, `personal`, `prod`, …

qfs stores the secret securely and **never prints it back**. The connection *name* is just metadata
(safe to show); the credential itself is write-only from your shell's perspective.

```sh
qfs connection add mail work
qfs connection add s3 prod
qfs connection add github personal
```

## Listing connections

```sh
qfs connection list            # all services
qfs connection list mail       # just one service
```

This prints **names and metadata only** — never a secret.

## Choosing the active connection

A service can have several connections (e.g. `work` and `personal` mail). Set which one is active:

```sh
qfs connection use mail work
```

## Removing a connection

```sh
qfs connection remove mail work     # idempotent — fine to run twice
```

## Least privilege

qfs is built so a credential is only ever used for the exact plan you commit, and never appears in
output, logs, or a `describe` report. On the **server**, you can go further and attach a
[`POLICY`](/server) that allows only specific verbs on specific paths — so an automation or an AI
agent can act, but only within the bounds you set:

```qfs
CREATE POLICY api ALLOW SELECT DENY INSERT, UPDATE, REMOVE, CALL
CREATE POLICY uploads ALLOW UPSERT ON 's3/*'
```

The guiding rule: grant the **minimum** a task needs, and let `preview` confirm a plan stays inside
those bounds before you commit.
