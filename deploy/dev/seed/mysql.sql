-- qfs dev seed (MySQL/MariaDB). Common column types; TINYINT(1) is the boolean convention.
CREATE TABLE widgets (
    id     INTEGER PRIMARY KEY,
    name   TEXT NOT NULL,
    qty    INTEGER,
    price  DOUBLE,
    active TINYINT(1)
);
INSERT INTO widgets (id, name, qty, price, active) VALUES
    (1, 'alpha', 10, 1.5, 1),
    (2, 'beta',  20, 2.5, 0),
    (3, 'gamma', 30, 3.5, 1);
