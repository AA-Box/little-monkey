import { describe, expect, it } from 'vitest';

import {
  applyWrite,
  buildProposalMessages,
  buildSchemaSummary,
  classifyStatement,
  dryRunWrite,
  extractTargetTable,
  introspectSchema,
  isPiiColumnName,
  isSingleStatement,
  openDatabaseFromBytes,
  parseProposalResponse,
  proposeSql,
  runSelect,
  suggestBackupPath,
  type DbProposalCallResult,
} from './dbAdminGuardrails';

async function makeTestDb() {
  const db = await openDatabaseFromBytes(new Uint8Array());
  db.run(`
    CREATE TABLE customers (
      id INTEGER PRIMARY KEY,
      full_name TEXT NOT NULL,
      email TEXT,
      notes TEXT
    );
  `);
  db.run(
    "INSERT INTO customers (full_name, email, notes) VALUES ('Ada Lovelace', 'ada@example.com', 'first customer')",
  );
  db.run(
    "INSERT INTO customers (full_name, email, notes) VALUES ('Alan Turing', 'alan@example.com', 'second customer')",
  );
  return db;
}

describe('classifyStatement', () => {
  it('classifies SELECT/WITH/PRAGMA/EXPLAIN as select', () => {
    expect(classifyStatement('select * from customers')).toBe('select');
    expect(classifyStatement('  WITH cte AS (SELECT 1) SELECT * FROM cte')).toBe('select');
    expect(classifyStatement('PRAGMA table_info(customers)')).toBe('select');
    expect(classifyStatement('EXPLAIN SELECT * FROM customers')).toBe('select');
  });

  it('classifies mutating statements as write', () => {
    for (const sql of [
      "insert into customers (full_name) values ('x')",
      "update customers set full_name = 'x' where id = 1",
      'delete from customers where id = 1',
      'alter table customers add column phone text',
      'drop table customers',
      'create table t (id integer)',
      "replace into customers (id, full_name) values (1, 'x')",
      'truncate table customers',
    ]) {
      expect(classifyStatement(sql)).toBe('write');
    }
  });

  it('classifies anything else as unsupported', () => {
    expect(classifyStatement('vacuum')).toBe('unsupported');
    expect(classifyStatement('')).toBe('unsupported');
  });
});

describe('isSingleStatement', () => {
  it('accepts one statement, with or without a trailing semicolon', () => {
    expect(isSingleStatement('select * from customers')).toBe(true);
    expect(isSingleStatement('select * from customers;')).toBe(true);
    expect(isSingleStatement('select * from customers;  ')).toBe(true);
  });

  it('rejects a smuggled second statement', () => {
    expect(isSingleStatement("select * from customers; drop table customers;")).toBe(false);
  });
});

describe('extractTargetTable', () => {
  it('extracts the table for every write shape', () => {
    expect(extractTargetTable("insert into customers (full_name) values ('x')")).toBe('customers');
    expect(extractTargetTable("update customers set full_name = 'x'")).toBe('customers');
    expect(extractTargetTable('delete from customers where id = 1')).toBe('customers');
    expect(extractTargetTable('alter table customers add column phone text')).toBe('customers');
    expect(extractTargetTable('drop table if exists customers')).toBe('customers');
    expect(extractTargetTable('create table if not exists widgets (id integer)')).toBe('widgets');
  });

  it('returns null for a SELECT', () => {
    expect(extractTargetTable('select * from customers')).toBeNull();
  });
});

describe('isPiiColumnName', () => {
  it('flags common PII-shaped column names', () => {
    expect(isPiiColumnName('full_name')).toBe(true);
    expect(isPiiColumnName('email')).toBe(true);
    expect(isPiiColumnName('phone_number')).toBe(true);
    expect(isPiiColumnName('ssn')).toBe(true);
    expect(isPiiColumnName('dateOfBirth')).toBe(true);
    expect(isPiiColumnName('home_address')).toBe(true);
  });

  it('does not flag ordinary non-PII column names', () => {
    expect(isPiiColumnName('id')).toBe(false);
    expect(isPiiColumnName('created_at')).toBe(false);
    expect(isPiiColumnName('quantity')).toBe(false);
    expect(isPiiColumnName('notes')).toBe(false);
  });
});

describe('introspectSchema + buildSchemaSummary', () => {
  it('introspects tables/columns and flags PII columns', async () => {
    const db = await makeTestDb();
    const tables = introspectSchema(db);
    expect(tables).toHaveLength(1);
    expect(tables[0].name).toBe('customers');
    const byName = Object.fromEntries(tables[0].columns.map((c) => [c.name, c]));
    expect(byName.id.primaryKey).toBe(true);
    expect(byName.full_name.pii).toBe(true);
    expect(byName.email.pii).toBe(true);
    expect(byName.notes.pii).toBe(false);

    const summary = buildSchemaSummary(tables);
    expect(summary).toContain('customers(');
    expect(summary).toContain('full_name');
    expect(summary).toContain('[possible PII]');
  });
});

describe('runSelect', () => {
  it('runs a read query and returns columns/rows', async () => {
    const db = await makeTestDb();
    const result = runSelect(db, 'SELECT id, full_name FROM customers ORDER BY id');
    expect(result.columns).toEqual(['id', 'full_name']);
    expect(result.rows).toEqual([
      [1, 'Ada Lovelace'],
      [2, 'Alan Turing'],
    ]);
  });

  it('returns empty columns/rows for a query with no result set', async () => {
    const db = await makeTestDb();
    const result = runSelect(db, "SELECT * FROM customers WHERE id = 999");
    expect(result.rows).toEqual([]);
  });
});

describe('dryRunWrite', () => {
  it('reports rows affected and PII columns, then leaves the database unchanged', async () => {
    const db = await makeTestDb();
    const tables = introspectSchema(db);
    const result = dryRunWrite(db, "UPDATE customers SET full_name = 'Changed' WHERE id = 1", tables);
    expect(result.rowsAffected).toBe(1);
    expect(result.targetTable).toBe('customers');
    expect(result.piiColumns).toEqual(expect.arrayContaining(['full_name', 'email']));

    // The dry run must always roll back — the row is untouched afterwards.
    const after = runSelect(db, 'SELECT full_name FROM customers WHERE id = 1');
    expect(after.rows).toEqual([['Ada Lovelace']]);
  });

  it('rolls back even when the statement itself throws', async () => {
    const db = await makeTestDb();
    const tables = introspectSchema(db);
    expect(() => dryRunWrite(db, 'INSERT INTO customers (id, full_name) VALUES (1, \'dupe\')', tables)).toThrow();
    // Still exactly 2 rows — the failed dry-run insert never stuck, and the
    // database is left in a usable (non-mid-transaction) state.
    const after = runSelect(db, 'SELECT COUNT(*) FROM customers');
    expect(after.rows).toEqual([[2]]);
  });

  it('reports no PII columns for a table with none', async () => {
    const db = await makeTestDb();
    db.run('CREATE TABLE widgets (id INTEGER PRIMARY KEY, quantity INTEGER)');
    const tables = introspectSchema(db);
    const result = dryRunWrite(db, 'DELETE FROM widgets', tables);
    expect(result.piiColumns).toEqual([]);
  });
});

describe('applyWrite', () => {
  it('actually commits the change', async () => {
    const db = await makeTestDb();
    const rowsAffected = applyWrite(db, "UPDATE customers SET full_name = 'Changed' WHERE id = 1");
    expect(rowsAffected).toBe(1);
    const after = runSelect(db, 'SELECT full_name FROM customers WHERE id = 1');
    expect(after.rows).toEqual([['Changed']]);
  });

  it('rolls back and rethrows on a failing statement', async () => {
    const db = await makeTestDb();
    expect(() => applyWrite(db, 'INSERT INTO customers (id, full_name) VALUES (1, \'dupe\')')).toThrow();
    const after = runSelect(db, 'SELECT COUNT(*) FROM customers');
    expect(after.rows).toEqual([[2]]);
  });
});

describe('suggestBackupPath', () => {
  it('appends a .bak-<timestamp> suffix to the original path', () => {
    const path = suggestBackupPath('/Users/me/data.sqlite');
    expect(path.startsWith('/Users/me/data.sqlite.bak-')).toBe(true);
    expect(path).not.toBe('/Users/me/data.sqlite');
  });
});

describe('buildProposalMessages', () => {
  it('includes the schema and the request text', () => {
    const messages = buildProposalMessages('customers(id INTEGER PK, full_name TEXT)', 'show me all customers');
    expect(messages).toHaveLength(2);
    expect(messages[0].role).toBe('system');
    expect(messages[1].role).toBe('user');
    expect(String(messages[1].content)).toContain('customers(id INTEGER PK, full_name TEXT)');
    expect(String(messages[1].content)).toContain('show me all customers');
  });
});

describe('parseProposalResponse', () => {
  it('parses a well-formed JSON reply', () => {
    const reply = JSON.stringify({ sql: 'SELECT * FROM customers', explanation: 'Lists every customer.' });
    expect(parseProposalResponse(reply)).toEqual({ sql: 'SELECT * FROM customers', explanation: 'Lists every customer.' });
  });

  it('extracts JSON embedded in surrounding prose', () => {
    const reply = `Sure thing! ${JSON.stringify({ sql: 'SELECT 1', explanation: 'A constant.' })} Hope that helps.`;
    expect(parseProposalResponse(reply)).toEqual({ sql: 'SELECT 1', explanation: 'A constant.' });
  });

  it('returns null when sql is missing', () => {
    expect(parseProposalResponse(JSON.stringify({ explanation: 'no sql here' }))).toBeNull();
  });

  it('returns null for unparseable content', () => {
    expect(parseProposalResponse('not json at all')).toBeNull();
  });
});

describe('proposeSql', () => {
  const okResult: DbProposalCallResult = {
    content: JSON.stringify({ sql: 'SELECT * FROM customers', explanation: 'Lists customers.' }),
    streamError: null,
  };

  it('returns a validated proposal on success', async () => {
    const callModel = async () => okResult;
    const proposal = await proposeSql('customers(...)', 'list customers', callModel);
    expect(proposal.sql).toBe('SELECT * FROM customers');
  });

  it('rejects an empty request before calling the model', async () => {
    const callModel = async () => okResult;
    await expect(proposeSql('customers(...)', '   ', callModel)).rejects.toThrow(/describe the query/i);
  });

  it('surfaces a stream error from the model call', async () => {
    const callModel = async (): Promise<DbProposalCallResult> => ({ content: '', streamError: 'model unavailable' });
    await expect(proposeSql('customers(...)', 'list customers', callModel)).rejects.toThrow('model unavailable');
  });

  it('rejects a proposal with more than one statement', async () => {
    const callModel = async (): Promise<DbProposalCallResult> => ({
      content: JSON.stringify({ sql: 'SELECT 1; DROP TABLE customers;', explanation: 'sneaky' }),
      streamError: null,
    });
    await expect(proposeSql('customers(...)', 'list customers', callModel)).rejects.toThrow(/single sql statement/i);
  });

  it('rejects an unparseable model reply', async () => {
    const callModel = async (): Promise<DbProposalCallResult> => ({ content: 'garbage', streamError: null });
    await expect(proposeSql('customers(...)', 'list customers', callModel)).rejects.toThrow(/usable SQL proposal/i);
  });
});
