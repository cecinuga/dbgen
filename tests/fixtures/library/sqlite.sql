-- Library in SQLite: a seeded lookup table, a many-to-many link as a WITHOUT ROWID table,
-- rowid keys, and loans whose dates follow each other.

CREATE TABLE genres (
    code TEXT PRIMARY KEY,
    name TEXT NOT NULL
);

INSERT INTO genres (code, name) VALUES
    ('fiction', 'Fiction'), ('history', 'History'), ('science', 'Science'), ('poetry', 'Poetry'), ('children', 'Children');

CREATE TABLE publishers (
    publisher_id INTEGER PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    city         TEXT
);

CREATE TABLE authors (
    author_id    INTEGER PRIMARY KEY AUTOINCREMENT,
    full_name    TEXT NOT NULL,
    birth_date   DATE,
    country_code TEXT
);

CREATE TABLE books (
    book_id      INTEGER PRIMARY KEY,
    isbn         TEXT NOT NULL UNIQUE,
    title        TEXT NOT NULL,
    publisher_id INTEGER NOT NULL REFERENCES publishers (publisher_id),
    genre_code   TEXT NOT NULL REFERENCES genres (code),
    language     TEXT NOT NULL CHECK (language IN ('en', 'it', 'fr', 'de', 'es')),
    published_on DATE NOT NULL,
    pages        INTEGER NOT NULL CHECK (pages BETWEEN 10 AND 2000),
    price        NUMERIC(8, 2) NOT NULL CHECK (price >= 0)
);

CREATE TABLE book_authors (
    book_id      INTEGER NOT NULL REFERENCES books (book_id) ON DELETE CASCADE,
    author_id    INTEGER NOT NULL REFERENCES authors (author_id),
    author_order INTEGER NOT NULL CHECK (author_order BETWEEN 1 AND 5),
    PRIMARY KEY (book_id, author_id)
) WITHOUT ROWID;

CREATE TABLE members (
    member_id  INTEGER PRIMARY KEY,
    email      TEXT NOT NULL,
    first_name TEXT NOT NULL,
    last_name  TEXT NOT NULL,
    joined_on  DATE NOT NULL,
    active     BOOLEAN NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX members_email ON members (email);

CREATE TABLE loans (
    loan_id     INTEGER PRIMARY KEY,
    book_id     INTEGER NOT NULL REFERENCES books (book_id),
    member_id   INTEGER NOT NULL REFERENCES members (member_id),
    loaned_at   DATETIME NOT NULL,
    due_at      DATETIME NOT NULL,
    returned_at DATETIME,
    CHECK (due_at > loaned_at),
    CHECK (returned_at IS NULL OR returned_at >= loaned_at)
);

CREATE INDEX loans_member ON loans (member_id);

CREATE VIEW overdue_loans AS
    SELECT loan_id, member_id, due_at FROM loans WHERE returned_at IS NULL AND due_at < CURRENT_TIMESTAMP;
