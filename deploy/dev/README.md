# qfs dev SQL stack (Postgres + MySQL)

A `podman compose` stack that runs Postgres + MariaDB with a seeded `widgets` table, so the live
`/sql` Postgres/MySQL backends can be developed and verified without external servers.

```sh
# 1. Bring up the databases (seeds `widgets` on first init).
podman-compose -f deploy/dev/compose.yml up -d

# 2. Point qfs at the example connections and query the live databases.
export QFS_CONNECTIONS=deploy/dev/connections.qfs
qfs run "/sql/pg/widgets |> where qty > 15 |> select name, qty |> order by qty desc"
qfs run "/sql/my/widgets |> where id == 1"

# 3. Tear the stack down (with volumes).
podman-compose -f deploy/dev/compose.yml down -v
```

## Connections

`connections.qfs` declares one connection per database. The dev URLs carry the password inline; in a
real deployment use an env-scheme secret reference instead of inlining it:

```
CREATE CONNECTION pg DRIVER postgres AT 'postgres://qfs@db.internal:5432/app' SECRET 'env:PG_PASSWORD';
```

> **Note:** the `connections.qfs` parser does not yet support `--` comments — keep these files
> comment-free until that lands (see ticket `20260630203060`'s Final Report). All guidance lives
> here in the README instead.

## Object storage (MinIO / `/s3`)

The stack also runs MinIO (S3-compatible) and seeds a `qfs-dev` bucket with two objects. The live
`/s3` reads are configured via env vars (the connection-model alignment is a follow-up):

```sh
export QFS_S3_REGION=us-east-1 QFS_S3_ACCESS_KEY_ID=qfs QFS_S3_BUCKET=qfs-dev \
       QFS_S3_ENDPOINT=http://localhost:9000 QFS_SECRET_S3_DEFAULT=qfs12345
qfs run "/s3/qfs-dev"                          # list objects
qfs run "/s3/qfs-dev/greeting.txt"             # download an object's content
qfs run "/s3/qfs-dev/data.csv |> decode csv"   # object -> content -> codec
```

MinIO uses path-style addressing, which qfs's SigV4 signer already emits. MinIO console: http://localhost:9001.

## Notes

- Passwords in this stack (`qfs`/`root`) are **dev-only**.
- A `vault:`-scheme secret reference needs the encrypted store's unlock flow and is not resolved at
  registry-build time yet — use `env:` or an inline dev URL here.
- Column-type coverage targets the common set (bool/int/float/text/bytes); richer types
  (NUMERIC/TIMESTAMP/UUID/JSON) are a follow-up.
