-- Version 3: the profile ("about me"), one row per Profile member.
-- Never edit this file once shipped; fix mistakes with a later version.
--
-- `key` is the Profile member name (full_name, emails, address, ...).
-- `value` is that member as JSON inside its own DPAPI envelope, so the table
-- never holds a readable name, address or date of birth. Rows are only
-- readable by the Windows user who wrote them.

CREATE TABLE profile_field (
    key          TEXT PRIMARY KEY,
    value        BLOB NOT NULL,
    updated_unix INTEGER NOT NULL
);
