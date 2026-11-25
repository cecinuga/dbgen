-- Blog in ClickHouse: threaded comments, Array and JSON columns, a MATERIALIZED column,
-- and tags as a ReplacingMergeTree link table whose sorting key must stay unique.

CREATE TABLE IF NOT EXISTS users
(
    user_id      UInt32,
    username     String,
    email        String,
    display_name String,
    bio          Nullable(String),
    is_admin     Bool DEFAULT false,
    created_at   DateTime('UTC')
)
ENGINE = ReplacingMergeTree
ORDER BY user_id;

CREATE TABLE IF NOT EXISTS posts
(
    post_id      UInt64,
    author_id    UInt32,
    title        String,
    slug         String,
    body         String,
    status       Enum8('draft' = 1, 'published' = 2, 'archived' = 3),
    created_at   DateTime('UTC'),
    published_at Nullable(DateTime('UTC')),
    keywords     Array(LowCardinality(String)),
    metadata     JSON,
    title_length UInt32 MATERIALIZED length(title),
    CONSTRAINT ck_published CHECK status != 'published' OR published_at IS NOT NULL,
    CONSTRAINT ck_published_after CHECK published_at IS NULL OR published_at >= created_at
)
ENGINE = ReplacingMergeTree
ORDER BY post_id;

CREATE TABLE IF NOT EXISTS comments
(
    comment_id        UInt64,
    post_id           UInt64,
    user_id           Nullable(UInt32),
    parent_comment_id Nullable(UInt64),
    body              String,
    created_at        DateTime('UTC'),
    upvotes           UInt32 DEFAULT 0
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(created_at)
ORDER BY (post_id, created_at, comment_id);

CREATE TABLE IF NOT EXISTS tags
(
    tag_id UInt16,
    name   String
)
ENGINE = ReplacingMergeTree
ORDER BY tag_id;

CREATE TABLE IF NOT EXISTS post_tags
(
    post_id UInt64,
    tag_id  UInt16
)
ENGINE = ReplacingMergeTree
ORDER BY (post_id, tag_id);
