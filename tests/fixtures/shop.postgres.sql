-- The shop of shop.sql in PostgreSQL. References are declared as foreign keys, so the
-- database checks every one of them during the load.

CREATE TYPE order_status AS ENUM ('new', 'paid', 'shipped', 'cancelled');

CREATE TABLE countries (
    code char(2) PRIMARY KEY,
    name text NOT NULL
);

INSERT INTO countries (code, name) VALUES ('IT', 'Italia'), ('FR', 'France'), ('DE', 'Deutschland');

-- A hierarchy: each category may have a parent category.
CREATE TABLE categories (
    category_id serial PRIMARY KEY,
    parent_id   integer REFERENCES categories (category_id),
    name        varchar(60) NOT NULL
);

CREATE TABLE customers (
    customer_id  bigserial PRIMARY KEY,
    email        varchar(120) NOT NULL UNIQUE,
    first_name   text NOT NULL,
    last_name    text,
    country_code char(2) NOT NULL REFERENCES countries (code),
    signup_at    timestamptz NOT NULL,
    closed_at    timestamptz,
    is_active    boolean NOT NULL,
    tags         text[],
    created_at   timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT ck_closed_after_signup CHECK (closed_at IS NULL OR closed_at > signup_at),
    CONSTRAINT ck_active_is_open CHECK (is_active = (closed_at IS NULL))
);

CREATE TABLE products (
    product_id  uuid PRIMARY KEY,
    category_id integer NOT NULL REFERENCES categories,
    sku         varchar(16) NOT NULL,
    title       text NOT NULL,
    price       numeric(10, 2) NOT NULL CHECK (price > 0 AND price <= 5000),
    discount    numeric(4, 3) NOT NULL,
    tax         numeric(4, 3) NOT NULL,
    net_share   numeric(4, 3) NOT NULL,
    attributes  jsonb,
    CONSTRAINT ck_shares CHECK (discount + tax + net_share = 1),
    CONSTRAINT ck_discount CHECK (discount BETWEEN 0 AND 0.5)
);

CREATE UNIQUE INDEX products_sku ON products (sku);

CREATE TABLE orders (
    order_id    bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    customer_id bigint NOT NULL REFERENCES customers,
    placed_at   timestamp(3) NOT NULL,
    channel     text NOT NULL,
    store_id    smallint,
    status      order_status NOT NULL,
    shipped_at  timestamp(3),
    CONSTRAINT ck_channel CHECK (CASE channel WHEN 'store' THEN store_id IS NOT NULL ELSE store_id IS NULL END),
    CONSTRAINT ck_shipped CHECK (shipped_at IS NULL OR shipped_at >= placed_at)
);

CREATE INDEX orders_placed_at ON orders (placed_at);

-- Later migrations.
ALTER TABLE orders ADD CONSTRAINT ck_channel_values CHECK (channel IN ('store', 'web', 'app'));
ALTER TABLE customers ADD COLUMN phone varchar(32);

CREATE TABLE order_lines (
    order_id   bigint NOT NULL REFERENCES orders ON DELETE CASCADE,
    product_id uuid NOT NULL REFERENCES products,
    quantity   smallint NOT NULL CHECK (quantity BETWEEN 1 AND 20),
    unit_price numeric(10, 2) NOT NULL,
    PRIMARY KEY (order_id, product_id)
);

-- Statements dbgen does not need to understand.
CREATE FUNCTION order_total(o bigint) RETURNS numeric LANGUAGE sql STABLE AS $$
    SELECT sum(quantity * unit_price) FROM order_lines WHERE order_id = o;
$$;

CREATE VIEW big_orders AS SELECT order_id FROM orders WHERE order_total(order_id) > 1000;

COMMENT ON TABLE orders IS 'One row per order';
