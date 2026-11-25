-- Library in ClickHouse: references are inferred from column names (book_id, author_id,
-- genre_code...), keys come from the sorting keys of ReplacingMergeTree tables, and loans
-- are partitioned by month.

CREATE TABLE IF NOT EXISTS genres
(
    code String,
    name String
)
ENGINE = ReplacingMergeTree
ORDER BY code;

INSERT INTO genres (code, name) VALUES
    ('fiction', 'Fiction'), ('history', 'History'), ('science', 'Science'), ('poetry', 'Poetry'), ('children', 'Children');

CREATE TABLE IF NOT EXISTS publishers
(
    publisher_id UInt32,
    name         String,
    city         LowCardinality(Nullable(String))
)
ENGINE = ReplacingMergeTree
ORDER BY publisher_id;

CREATE TABLE IF NOT EXISTS authors
(
    author_id    UInt32,
    full_name    String,
    birth_date   Nullable(Date32),
    country_code Nullable(FixedString(2))
)
ENGINE = ReplacingMergeTree
ORDER BY author_id;

CREATE TABLE IF NOT EXISTS books
(
    book_id      UInt64,
    isbn         FixedString(13),
    title        String,
    publisher_id UInt32,
    genre_code   LowCardinality(String),
    language     Enum8('en' = 1, 'it' = 2, 'fr' = 3, 'de' = 4, 'es' = 5),
    published_on Date,
    pages        UInt16,
    price        Decimal(8, 2),
    keywords     Array(String),
    CONSTRAINT ck_pages CHECK pages BETWEEN 10 AND 2000,
    CONSTRAINT ck_price CHECK price >= 0
)
ENGINE = ReplacingMergeTree
ORDER BY book_id;

CREATE TABLE IF NOT EXISTS book_authors
(
    book_id      UInt64,
    author_id    UInt32,
    author_order UInt8,
    CONSTRAINT ck_author_order CHECK author_order BETWEEN 1 AND 5
)
ENGINE = ReplacingMergeTree
ORDER BY (book_id, author_id);

CREATE TABLE IF NOT EXISTS members
(
    member_id  UInt32,
    email      String,
    first_name String,
    last_name  String,
    joined_on  Date,
    active     Bool,
    created_at DateTime DEFAULT now()
)
ENGINE = ReplacingMergeTree
ORDER BY member_id;

CREATE TABLE IF NOT EXISTS loans
(
    loan_id     UInt64,
    book_id     UInt64,
    member_id   UInt32,
    loaned_at   DateTime64(3, 'UTC'),
    due_at      DateTime64(3, 'UTC'),
    returned_at Nullable(DateTime64(3, 'UTC')),
    days_out    Nullable(Int64) MATERIALIZED dateDiff('day', loaned_at, returned_at),
    CONSTRAINT ck_due CHECK due_at > loaned_at,
    CONSTRAINT ck_returned CHECK returned_at IS NULL OR returned_at >= loaned_at
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(loaned_at)
ORDER BY (loaned_at, loan_id);

ALTER TABLE members ADD COLUMN IF NOT EXISTS phone Nullable(String) AFTER email;
