
        BEGIN IMMEDIATE;
        CREATE TABLE IF NOT EXISTS records(id TEXT PRIMARY KEY,scope TEXT NOT NULL,kind TEXT NOT NULL,
          revision INTEGER NOT NULL,status TEXT NOT NULL,expires INTEGER,doc TEXT NOT NULL,fact_key TEXT);
        CREATE INDEX IF NOT EXISTS records_scope ON records(scope,status,kind);
        CREATE INDEX IF NOT EXISTS records_fact ON records(scope,fact_key);
        CREATE TABLE IF NOT EXISTS versions(id TEXT,revision INTEGER,doc TEXT NOT NULL,PRIMARY KEY(id,revision));
        CREATE VIRTUAL TABLE IF NOT EXISTS search_index USING fts5(id UNINDEXED, words);
        CREATE TABLE IF NOT EXISTS operations(id TEXT PRIMARY KEY,principal TEXT,scope TEXT,key TEXT,
          digest TEXT,result TEXT,UNIQUE(principal,scope,key));
        CREATE TABLE IF NOT EXISTS observations(id TEXT PRIMARY KEY,scope TEXT,source_key TEXT,digest TEXT,
          payload TEXT,state TEXT,record_id TEXT,UNIQUE(scope,source_key,digest));
        CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY,state TEXT NOT NULL,attempts INTEGER NOT NULL,
          next_at INTEGER NOT NULL,error TEXT);
        CREATE TABLE IF NOT EXISTS dependencies(record_id TEXT,source_key TEXT,PRIMARY KEY(record_id,source_key));
        CREATE TABLE IF NOT EXISTS tombstones(scope TEXT,source_key TEXT,PRIMARY KEY(scope,source_key));
        CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
        PRAGMA user_version=1;
        COMMIT;
        