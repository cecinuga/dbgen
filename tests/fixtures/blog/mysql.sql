-- Blog in MySQL: threaded comments (a nullable self-reference), a CHECK that ties two
-- columns together, JSON columns, and tags as a many-to-many link.

CREATE TABLE users (
    user_id      INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    username     VARCHAR(30) NOT NULL,
    email        VARCHAR(120) NOT NULL,
    display_name VARCHAR(80) NOT NULL,
    bio          TEXT NULL,
    is_admin     BOOLEAN NOT NULL DEFAULT FALSE,
    created_at   DATETIME NOT NULL,
    UNIQUE KEY uk_users_username (username),
    UNIQUE KEY uk_users_email (email)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE posts (
    post_id      BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    author_id    INT NOT NULL,
    title        VARCHAR(200) NOT NULL,
    slug         VARCHAR(120) NOT NULL,
    body         MEDIUMTEXT NOT NULL,
    status       ENUM('draft', 'published', 'archived') NOT NULL,
    created_at   DATETIME NOT NULL,
    published_at DATETIME NULL,
    metadata     JSON NULL,
    UNIQUE KEY uk_posts_slug (slug),
    CONSTRAINT fk_posts_author FOREIGN KEY (author_id) REFERENCES users (user_id),
    CONSTRAINT ck_published CHECK (status <> 'published' OR published_at IS NOT NULL),
    CONSTRAINT ck_published_after CHECK (published_at IS NULL OR published_at >= created_at)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE comments (
    comment_id        BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    post_id           BIGINT NOT NULL,
    user_id           INT NULL,
    parent_comment_id BIGINT NULL,
    body              TEXT NOT NULL,
    created_at        DATETIME NOT NULL,
    upvotes           INT NOT NULL DEFAULT 0,
    INDEX idx_comments_post (post_id, created_at),
    CONSTRAINT fk_comments_post FOREIGN KEY (post_id) REFERENCES posts (post_id) ON DELETE CASCADE,
    CONSTRAINT fk_comments_user FOREIGN KEY (user_id) REFERENCES users (user_id) ON DELETE SET NULL,
    CONSTRAINT fk_comments_parent FOREIGN KEY (parent_comment_id) REFERENCES comments (comment_id),
    CONSTRAINT ck_upvotes CHECK (upvotes >= 0)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE tags (
    tag_id SMALLINT NOT NULL PRIMARY KEY,
    name   VARCHAR(40) NOT NULL UNIQUE
) ENGINE = InnoDB;

CREATE TABLE post_tags (
    post_id BIGINT NOT NULL,
    tag_id  SMALLINT NOT NULL,
    PRIMARY KEY (post_id, tag_id),
    CONSTRAINT fk_pt_post FOREIGN KEY (post_id) REFERENCES posts (post_id) ON DELETE CASCADE,
    CONSTRAINT fk_pt_tag FOREIGN KEY (tag_id) REFERENCES tags (tag_id)
) ENGINE = InnoDB;

-- Later migrations.
ALTER TABLE users ADD COLUMN last_login_at DATETIME NULL AFTER created_at;
