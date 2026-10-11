-- A table rebuilt the way ORMs do it, so the parent ends up after the
-- table that references it; a generated column; a name LIKE '_cf_%' matches.
CREATE TABLE authors(id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE posts(id INTEGER PRIMARY KEY, author INTEGER NOT NULL REFERENCES authors(id), title TEXT, slug TEXT GENERATED ALWAYS AS (lower(title)) VIRTUAL);
CREATE TABLE authors_new(id INTEGER PRIMARY KEY, name TEXT);
DROP TABLE authors;
ALTER TABLE authors_new RENAME TO authors;
CREATE TABLE acf_fields(k TEXT);
