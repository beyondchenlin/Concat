-- Pinned Pi 1.5.2 schema initialization contract fixture; never used by production.
-- Source: packages/durable/src/storage/sqlite/migrations.ts and chord/src/sqlite/migrations.ts
-- Commit: d36266e06669c6991a7c9509d1da04d18d30378d
-- SPDX-License-Identifier: MIT
-- MIT License
--
-- Copyright (c) 2025 Mario Zechner
--
-- Permission is hereby granted, free of charge, to any person obtaining a copy
-- of this software and associated documentation files (the "Software"), to deal
-- in the Software without restriction, including without limitation the rights
-- to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
-- copies of the Software, and to permit persons to whom the Software is
-- furnished to do so, subject to the following conditions:
--
-- The above copyright notice and this permission notice shall be included in all
-- copies or substantial portions of the Software.
--
-- THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
-- IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
-- FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
-- AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
-- LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
-- OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
-- SOFTWARE.

CREATE TABLE IF NOT EXISTS durable_schema (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    version INTEGER NOT NULL CHECK (version >= 0)
) STRICT;
INSERT OR IGNORE INTO durable_schema (singleton, version) VALUES (1, 0);

CREATE TABLE durable_metadata (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    next_id TEXT NOT NULL,
    next_seq INTEGER NOT NULL
) STRICT;
INSERT INTO durable_metadata (singleton, next_id, next_seq) VALUES (1, '2', 1);
CREATE TABLE record_ids (
    id INTEGER PRIMARY KEY,
    record_type TEXT NOT NULL CHECK (record_type IN ('conversation', 'entry', 'task', 'submission', 'document'))
) STRICT;
CREATE TABLE conversations (
    id INTEGER PRIMARY KEY,
    owner_conversation_id INTEGER,
    owner_task_id INTEGER,
    record TEXT NOT NULL CHECK (json_valid(record))
) STRICT;
CREATE INDEX conversations_by_owner_conversation ON conversations (owner_conversation_id, id);
CREATE INDEX conversations_by_owner_task ON conversations (owner_task_id, id);
CREATE TABLE entries (
    id INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL,
    head INTEGER,
    commit_seq INTEGER NOT NULL,
    record TEXT NOT NULL CHECK (json_valid(record))
) STRICT;
CREATE INDEX entries_by_conversation ON entries (conversation_id, id DESC);
CREATE INDEX entry_heads_by_conversation ON entries (conversation_id, id DESC) WHERE head IS NOT NULL;
CREATE TABLE tasks (
    id INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'waiting', 'completing', 'terminal')),
    abort_requested INTEGER NOT NULL CHECK (abort_requested IN (0, 1)),
    background INTEGER NOT NULL CHECK (background IN (0, 1)),
    record TEXT NOT NULL CHECK (json_valid(record))
) STRICT;
CREATE INDEX tasks_by_status ON tasks (status, id);
CREATE INDEX tasks_by_conversation ON tasks (conversation_id, id);
CREATE INDEX tasks_by_kind ON tasks (kind, id);
CREATE INDEX tasks_by_abort_requested ON tasks (abort_requested, id);
CREATE INDEX tasks_by_background ON tasks (background, id);
CREATE TABLE submissions (
    id INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL,
    request_id TEXT,
    status TEXT NOT NULL CHECK (status IN ('queued', 'placed', 'done', 'unanswered')),
    record TEXT NOT NULL CHECK (json_valid(record))
) STRICT;
CREATE INDEX submissions_by_request ON submissions (conversation_id, request_id);
CREATE INDEX submissions_by_conversation ON submissions (conversation_id, id);
CREATE INDEX submissions_by_status ON submissions (status, id);
CREATE TABLE documents (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    family INTEGER NOT NULL CHECK (family IN (0, 1)),
    key_value TEXT NOT NULL,
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('session', 'conversation', 'task')),
    owner_id INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    retired_at INTEGER,
    record TEXT NOT NULL CHECK (json_valid(record))
) STRICT;
CREATE INDEX documents_by_address
    ON documents (kind, scope_kind, owner_id, family, key_value, created_at DESC, retired_at);
CREATE INDEX documents_by_scope ON documents (scope_kind, owner_id, id);
CREATE INDEX documents_by_scope_kind ON documents (scope_kind, owner_id, kind, id);
CREATE TABLE document_revisions (
    document_id INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('base', 'delta')),
    version INTEGER NOT NULL,
    content TEXT NOT NULL CHECK (json_valid(content)),
    PRIMARY KEY (document_id, seq)
) STRICT;
CREATE INDEX document_revisions_by_kind ON document_revisions (document_id, kind, seq DESC);
UPDATE durable_schema SET version = 1 WHERE singleton = 1;

CREATE TABLE tasks_v2 (
    id INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'waiting', 'sleeping', 'completing', 'terminal')),
    abort_requested INTEGER NOT NULL CHECK (abort_requested IN (0, 1)),
    background INTEGER NOT NULL CHECK (background IN (0, 1)),
    record TEXT NOT NULL CHECK (json_valid(record))
) STRICT;
INSERT INTO tasks_v2 (id, conversation_id, kind, status, abort_requested, background, record)
    SELECT id, conversation_id, kind, status, abort_requested, background, record FROM tasks;
DROP TABLE tasks;
ALTER TABLE tasks_v2 RENAME TO tasks;
CREATE INDEX tasks_by_status ON tasks (status, id);
CREATE INDEX tasks_by_conversation ON tasks (conversation_id, id);
CREATE INDEX tasks_by_kind ON tasks (kind, id);
CREATE INDEX tasks_by_abort_requested ON tasks (abort_requested, id);
CREATE INDEX tasks_by_background ON tasks (background, id);
UPDATE durable_schema SET version = 2 WHERE singleton = 1;
