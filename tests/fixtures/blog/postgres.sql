-- Blog in PostgreSQL: threaded comments (a nullable self-reference), a CHECK that ties
-- two columns together, jsonb and text[] columns, and tags as a many-to-many link.

CREATE TYPE post_status AS ENUM ('draft', 'published', 'archived');
CREATE DOMAIN username AS varchar(30);

CREATE TABLE users (
    user_id      integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    username     username NOT NULL UNIQUE,
    email        varchar(120) NOT NULL UNIQUE,
    display_name text NOT NULL,
    bio          text,
    is_admin     boolean NOT NULL DEFAULT false,
    created_at   timestamptz NOT NULL
);

CREATE TABLE posts (
    post_id      bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    author_id    integer NOT NULL REFERENCES users (user_id),
    title        varchar(200) NOT NULL,
    slug         varchar(120) NOT NULL UNIQUE,
    body         text NOT NULL,
    status       post_status NOT NULL,
    created_at   timestamptz NOT NULL,
    published_at timestamptz,
    keywords     text[],
    metadata     jsonb,
    CONSTRAINT ck_published CHECK (status <> 'published' OR published_at IS NOT NULL),
    CONSTRAINT ck_published_after CHECK (published_at IS NULL OR published_at >= created_at)
);

CREATE TABLE comments (
    comment_id        bigserial PRIMARY KEY,
    post_id           bigint NOT NULL REFERENCES posts ON DELETE CASCADE,
    user_id           integer REFERENCES users ON DELETE SET NULL,
    parent_comment_id bigint REFERENCES comments (comment_id),
    body              text NOT NULL,
    created_at        timestamptz NOT NULL,
    upvotes           integer NOT NULL DEFAULT 0 CHECK (upvotes >= 0)
);

CREATE TABLE tags (
    tag_id smallint PRIMARY KEY,
    name   varchar(40) NOT NULL UNIQUE
);

CREATE TABLE post_tags (
    post_id bigint NOT NULL REFERENCES posts ON DELETE CASCADE,
    tag_id  smallint NOT NULL REFERENCES tags,
    PRIMARY KEY (post_id, tag_id)
);

CREATE INDEX comments_by_post ON comments (post_id, created_at);
CREATE UNIQUE INDEX users_email_lower ON users (lower(email));

CREATE FUNCTION touch_post() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.metadata := coalesce(NEW.metadata, '{}'::jsonb) || jsonb_build_object('touched', now());
    RETURN NEW;
END;
$$;

CREATE TRIGGER posts_touch BEFORE UPDATE ON posts FOR EACH ROW EXECUTE FUNCTION touch_post();
