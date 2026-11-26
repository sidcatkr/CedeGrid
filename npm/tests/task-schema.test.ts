/** 003 FR-060: `commandTask` takes exactly the task schema's fields, by their TypeScript names. */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { COMMAND_TASK_FIELDS } from '../src/client';

test('commandTask takes every schema field', () => {
    const schema = JSON.parse(readFileSync(join(__dirname, '..', '..', '..', 'tests', 'contracts', 'task-schema.json'), 'utf8'));
    const names = (schema.fields as { typescript: string }[]).map(field => field.typescript).sort();
    assert.deepEqual([...COMMAND_TASK_FIELDS].sort(), names);
});
