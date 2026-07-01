-- qfs dev seed (Postgres). Common column types so the live `/sql` backend's type mapping is exercised.
CREATE TABLE widgets (
    id     INTEGER PRIMARY KEY,
    name   TEXT NOT NULL,
    qty    INTEGER,
    price  DOUBLE PRECISION,
    active BOOLEAN
);
INSERT INTO widgets (id, name, qty, price, active) VALUES
    (1, 'alpha', 10, 1.5, true),
    (2, 'beta',  20, 2.5, false),
    (3, 'gamma', 30, 3.5, true);
