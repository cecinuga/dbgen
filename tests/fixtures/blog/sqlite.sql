-- Blog in SQLite: threaded comments (a nullable self-reference), a CHECK that ties two
-- columns together, a trigger with a multi-statement body, and tags as a many-to-many link.

CREATE TABLE users (
    user_id      INTEGER PRIMARY KEY,
    username     TEXT NOT NULL UNIQUE,
    email        TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    bio          TEXT,
    is_admin     BOOLEAN NOT NULL DEFAULT 0,
    created_at   DATETIME NOT NULL
);

CREATE TABLE posts (
    post_id      INTEGER PRIMARY KEY,
    author_id    INTEGER NOT NULL REFERENCES users (user_id),
    title        TEXT NOT NULL,
    slug         TEXT NOT NULL UNIQUE,
    body         TEXT NOT NULL,
    status       TEXT NOT NULL CHECK (status IN ('draft', 'published', 'archived')),
    created_at   DATETIME NOT NULL,
    published_at DATETIME,
    CHECK (status <> 'published' OR published_at IS NOT NULL),
    CHECK (published_at IS NULL OR published_at >= created_at)
);

CREATE TABLE comments (
    comment_id        INTEGER PRIMARY KEY,
    post_id           INTEGER NOT NULL REFERENCES posts (post_id) ON DELETE CASCADE,
    user_id           INTEGER REFERENCES users (user_id) ON DELETE SET NULL,
    parent_comment_id INTEGER REFERENCES comments (comment_id),
    body              TEXT NOT NULL,
    created_at        DATETIME NOT NULL,
    upvotes           INTEGER NOT NULL DEFAULT 0 CHECK (upvotes >= 0)
);

CREATE TABLE tags (
    tag_id INTEGER PRIMARY KEY,
    name   TEXT NOT NULL UNIQUE
);

CREATE TABLE post_tags (
    post_id INTEGER NOT NULL REFERENCES posts (post_id) ON DELETE CASCADE,
    tag_id  INTEGER NOT NULL REFERENCES tags (tag_id),
    PRIMARY KEY (post_id, tag_id)
) WITHOUT ROWID;

CREATE INDEX comments_by_post ON comments (post_id, created_at);

CREATE TABLE comment_audit (
    comment_id INTEGER NOT NULL,
    changed_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TRIGGER comments_audit AFTER UPDATE OF body ON comments
BEGIN
    INSERT INTO comment_audit (comment_id) VALUES (NEW.comment_id);
    UPDATE comments SET upvotes = 0 WHERE comment_id = NEW.comment_id AND upvotes < 0;
END;
