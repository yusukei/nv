-- Values a JSON backup loses unless it takes care: integers beyond 2^53, a
-- real 1.0 in a column without a type, text starting with char(1), a
-- hidden rowid, and an AUTOINCREMENT counter past deleted rows.
CREATE TABLE big(n INTEGER, x, t TEXT);
CREATE TABLE seqd(id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT);
