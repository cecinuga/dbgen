-- The shop of shop.sql in SQLite. References are declared as foreign keys; dbgen turns
-- their checks on, so SQLite verifies every one of them during the load.
--
-- SQLite stores decimals as floating point, so shop.sql's exact-sum CHECK
-- (discount + tax + net_share = 1) has no equivalent here.

PRAGMA foreign_keys = ON;

CREATE TABLE countries (
    code TEXT PRIMARY KEY,
    name TEXT NOT NULL
);

INSERT INTO countries (code, name) VALUES ('IT', 'Italia'), ('FR', 'France'), ('DE', 'Deutschland');

-- A hierarchy: each category may have a parent category.
CREATE TABLE categories (
    category_id INTEGER PRIMARY KEY,
    parent_id   INTEGER REFERENCES categories (category_id),
    name        TEXT NOT NULL
);

CREATE TABLE customers (
    customer_id  INTEGER PRIMARY KEY AUTOINCREMENT,
    email        TEXT NOT NULL UNIQUE,
    first_name   TEXT NOT NULL,
    last_name    TEXT,
    country_code TEXT NOT NULL REFERENCES countries (code),
    signup_at    DATETIME NOT NULL,
    closed_at    DATETIME,
    is_active    BOOLEAN NOT NULL,
    note,
    CHECK (closed_at IS NULL OR closed_at > signup_at),
    CHECK (is_active = (closed_at IS NULL))
);

CREATE TABLE products (
    product_id  TEXT PRIMARY KEY,
    category_id INTEGER NOT NULL REFERENCES categories (category_id),
    sku         TEXT NOT NULL UNIQUE,
    title       TEXT NOT NULL,
    price       NUMERIC(10, 2) NOT NULL CHECK (price > 0 AND price <= 5000),
    discount    NUMERIC(4, 3) NOT NULL CHECK (discount BETWEEN 0 AND 0.5)
);

CREATE TABLE orders (
    order_id    INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customers (customer_id),
    placed_at   DATETIME NOT NULL,
    channel     TEXT NOT NULL CHECK (channel IN ('store', 'web', 'app')),
    store_id    INTEGER,
    status      TEXT NOT NULL CHECK (status IN ('new', 'paid', 'shipped', 'cancelled')),
    shipped_at  DATETIME,
    CHECK (CASE channel WHEN 'store' THEN store_id IS NOT NULL ELSE store_id IS NULL END),
    CHECK (shipped_at IS NULL OR shipped_at >= placed_at)
);

CREATE INDEX orders_placed_at ON orders (placed_at);

CREATE TABLE order_lines (
    order_id   INTEGER NOT NULL REFERENCES orders (order_id),
    product_id TEXT NOT NULL REFERENCES products (product_id),
    quantity   INTEGER NOT NULL CHECK (quantity BETWEEN 1 AND 20),
    unit_price NUMERIC(10, 2) NOT NULL,
    PRIMARY KEY (order_id, product_id)
);

-- Statements dbgen does not need to understand. The trigger body holds semicolons.
CREATE VIEW big_orders AS SELECT order_id FROM order_lines GROUP BY order_id HAVING SUM(quantity * unit_price) > 1000;

CREATE TRIGGER orders_shipped AFTER UPDATE OF status ON orders
WHEN NEW.status = 'shipped' AND NEW.shipped_at IS NULL
BEGIN
    UPDATE orders SET shipped_at = CURRENT_TIMESTAMP WHERE order_id = NEW.order_id;
    SELECT CASE WHEN NEW.store_id IS NULL THEN 'web' ELSE 'store' END;
END;
