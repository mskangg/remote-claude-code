import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { recordStopHookEvent, readStableAssistantEntryForStop } from './claude-stop-hook.mjs';
import { appendHookEvent } from './hook-event-store.mjs';

test('recordStopHookEvent skips duplicate stop events for the same assistant turn', async () => {
    const tempDir = await mkdtemp(join(tmpdir(), 'claude-stop-hook-'));
    const hookEventFilePath = join(tempDir, 'events.jsonl');

    await recordStopHookEvent({
        hookEventName: 'Stop',
        sessionId: 'session-1',
        turnId: 'session-1:assistant-uuid-1',
        hookEventFilePath,
        text: '첫 번째 응답',
    });

    await recordStopHookEvent({
        hookEventName: 'Stop',
        sessionId: 'session-1',
        turnId: 'session-1:assistant-uuid-1',
        hookEventFilePath,
        text: '첫 번째 응답',
    });

    const lines = (await readFile(hookEventFilePath, 'utf8'))
        .split('\n')
        .filter((line) => line.trim().length > 0);

    assert.equal(lines.length, 1);
    const [event] = lines.map((line) => JSON.parse(line));
    assert.equal(event.turnId, 'session-1:assistant-uuid-1');
    assert.equal(event.text, '첫 번째 응답');
});

test('recordStopHookEvent keeps distinct stop events for different assistant turns', async () => {
    const tempDir = await mkdtemp(join(tmpdir(), 'claude-stop-hook-'));
    const hookEventFilePath = join(tempDir, 'events.jsonl');

    await recordStopHookEvent({
        hookEventName: 'Stop',
        sessionId: 'session-1',
        turnId: 'session-1:assistant-uuid-1',
        hookEventFilePath,
        text: '첫 번째 응답',
    });

    await recordStopHookEvent({
        hookEventName: 'Stop',
        sessionId: 'session-1',
        turnId: 'session-1:assistant-uuid-2',
        hookEventFilePath,
        text: '두 번째 응답',
    });

    const lines = (await readFile(hookEventFilePath, 'utf8'))
        .split('\n')
        .filter((line) => line.trim().length > 0);

    assert.equal(lines.length, 2);
    const events = lines.map((line) => JSON.parse(line));
    assert.deepEqual(
        events.map((event) => event.turnId),
        ['session-1:assistant-uuid-1', 'session-1:assistant-uuid-2'],
    );
});

test('readStableAssistantEntryForStop waits for a new assistant turn beyond the last delivered stop', async () => {
    const tempDir = await mkdtemp(join(tmpdir(), 'claude-stop-hook-'));
    const hookEventFilePath = join(tempDir, 'events.jsonl');
    const transcriptPath = join(tempDir, 'transcript.jsonl');

    await appendHookEvent(hookEventFilePath, {
        sessionId: 'remote-session-1',
        turnId: 'payload-session-1:assistant-uuid-1',
        event: 'Stop',
        text: '첫 번째 응답',
        createdAt: new Date().toISOString(),
    });

    const firstAssistant = JSON.stringify({
        type: 'assistant',
        uuid: 'assistant-uuid-1',
        message: {
            role: 'assistant',
            content: [{ type: 'text', text: '첫 번째 응답' }],
            stop_reason: 'end_turn',
        },
    });
    const secondAssistant = JSON.stringify({
        type: 'assistant',
        uuid: 'assistant-uuid-2',
        message: {
            role: 'assistant',
            content: [{ type: 'text', text: '두 번째 응답' }],
            stop_reason: 'end_turn',
        },
    });

    await import('node:fs/promises').then(({ writeFile }) => writeFile(transcriptPath, `${firstAssistant}\n`, 'utf8'));

    setTimeout(async () => {
        const { writeFile } = await import('node:fs/promises');
        await writeFile(transcriptPath, `${firstAssistant}\n${secondAssistant}\n`, 'utf8');
    }, 50);

    const entry = await readStableAssistantEntryForStop({
        relaySessionId: 'remote-session-1',
        payloadSessionId: 'payload-session-1',
        transcriptPath,
        hookEventFilePath,
    });

    assert.equal(entry?.uuid, 'assistant-uuid-2');
    assert.equal(entry?.text, '두 번째 응답');
});

test('hook runtime should not rely on cwd-based project root fallback', async () => {
    const source = await readFile(new URL('./claude-stop-hook.mjs', import.meta.url), 'utf8');
    assert.equal(source.includes('process.cwd()'), false);
});
