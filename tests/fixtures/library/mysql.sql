-- Library in MySQL: a seeded lookup table, a many-to-many link with a composite key,
-- ENUM and JSON columns, AUTO_INCREMENT keys, and loans whose dates follow each other.

CREATE TABLE genres (
    code VARCHAR(12) NOT NULL PRIMARY KEY,
    name VARCHAR(60) NOT NULL
) ENGINE = InnoDB;

INSERT INTO genres (code, name) VALUES
    ('fiction', 'Fiction'), ('history', 'History'), ('science', 'Science'), ('poetry', 'Poetry'), ('children', 'Children');

CREATE TABLE publishers (
    publisher_id INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    name         VARCHAR(80) NOT NULL,
    city         VARCHAR(60) NULL,
    UNIQUE KEY uk_publishers_name (name)
) ENGINE = InnoDB;

CREATE TABLE authors (
    author_id    INT UNSIGNED NOT NULL AUTO_INCREMENT,
    full_name    VARCHAR(120) NOT NULL,
    birth_date   DATE NULL,
    country_code CHAR(2) NULL,
    PRIMARY KEY (author_id)
) ENGINE = InnoDB;

CREATE TABLE books (
    book_id      BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    isbn         CHAR(13) NOT NULL,
    title        VARCHAR(200) NOT NULL,
    publisher_id INT NOT NULL,
    genre_code   VARCHAR(12) NOT NULL,
    language     ENUM('en', 'it', 'fr', 'de', 'es') NOT NULL,
    published_on DATE NOT NULL,
    pages        INT NOT NULL,
    price        DECIMAL(8, 2) NOT NULL,
    keywords     JSON NULL,
    UNIQUE KEY uk_books_isbn (isbn),
    INDEX idx_books_published (published_on),
    CONSTRAINT fk_books_publisher FOREIGN KEY (publisher_id) REFERENCES publishers (publisher_id),
    CONSTRAINT fk_books_genre FOREIGN KEY (genre_code) REFERENCES genres (code),
    CONSTRAINT ck_pages CHECK (pages BETWEEN 10 AND 2000),
    CONSTRAINT ck_price CHECK (price >= 0)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE book_authors (
    book_id      BIGINT NOT NULL,
    author_id    INT UNSIGNED NOT NULL,
    author_order TINYINT NOT NULL,
    PRIMARY KEY (book_id, author_id),
    CONSTRAINT fk_ba_book FOREIGN KEY (book_id) REFERENCES books (book_id) ON DELETE CASCADE,
    CONSTRAINT fk_ba_author FOREIGN KEY (author_id) REFERENCES authors (author_id),
    CONSTRAINT ck_author_order CHECK (author_order BETWEEN 1 AND 5)
) ENGINE = InnoDB;

CREATE TABLE members (
    member_id  INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    email      VARCHAR(120) NOT NULL,
    first_name VARCHAR(60) NOT NULL,
    last_name  VARCHAR(60) NOT NULL,
    joined_on  DATE NOT NULL,
    active     BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
) ENGINE = InnoDB;

CREATE UNIQUE INDEX uk_members_email ON members (email);

CREATE TABLE loans (
    loan_id     BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    book_id     BIGINT NOT NULL,
    member_id   INT NOT NULL,
    loaned_at   DATETIME NOT NULL,
    due_at      DATETIME NOT NULL,
    returned_at DATETIME NULL,
    CONSTRAINT fk_loans_member FOREIGN KEY (member_id) REFERENCES members (member_id),
    CONSTRAINT ck_due CHECK (due_at > loaned_at),
    CONSTRAINT ck_returned CHECK (returned_at IS NULL OR returned_at >= loaned_at)
) ENGINE = InnoDB;

-- Later migrations.
ALTER TABLE loans ADD CONSTRAINT fk_loans_book FOREIGN KEY (book_id) REFERENCES books (book_id);
ALTER TABLE members MODIFY COLUMN active BOOLEAN NOT NULL;

CREATE VIEW overdue_loans AS
    SELECT loan_id, member_id, due_at FROM loans WHERE returned_at IS NULL AND due_at < NOW();
