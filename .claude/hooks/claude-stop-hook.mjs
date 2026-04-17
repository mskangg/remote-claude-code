import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { appendHookEvent, readHookEvents } from './hook-event-store.mjs';
const TERMINAL_EVENTS = new Set(['Stop', 'StopFailure']);
const buildAssistantTurnId = (sessionId, assistantUuid) => {
    if (!assistantUuid) {
        return null;
    }
    return `${sessionId}:${assistantUuid}`;
};
const findLastTerminalTurnId = async (hookEventFilePath, sessionId) => {
    const events = await readHookEvents(hookEventFilePath);
    return (events
        .filter((event) => event.sessionId === sessionId && TERMINAL_EVENTS.has(event.event))
        .at(-1)?.turnId ?? null);
};
export const recordStopHookEvent = async (input) => {
    if (TERMINAL_EVENTS.has(input.hookEventName)) {
        const events = await readHookEvents(input.hookEventFilePath);
        const lastAssistantEvent = events
            .filter((event) => event.sessionId === input.sessionId &&
            TERMINAL_EVENTS.has(event.event))
            .at(-1);
        if (lastAssistantEvent?.turnId === input.turnId) {
            return;
        }
    }
    await appendHookEvent(input.hookEventFilePath, {
        sessionId: input.sessionId,
        turnId: input.turnId,
        event: input.hookEventName,
        text: input.text,
        createdAt: new Date().toISOString(),
    });
};
const readStdin = async () => {
    const chunks = [];
    for await (const chunk of process.stdin) {
        chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
    }
    return Buffer.concat(chunks).toString('utf8');
};
const extractTextFromContent = (content) => {
    if (typeof content === 'string') {
        return content.trim();
    }
    if (!Array.isArray(content)) {
        return '';
    }
    return content
        .flatMap((item) => {
        if (!item || typeof item !== 'object') {
            return [];
        }
        const candidate = item;
        if (candidate.type !== 'text' || typeof candidate.text !== 'string') {
            return [];
        }
        return [candidate.text];
    })
        .join('\n')
        .trim();
};
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const readLatestAssistantEntryFromTranscript = async (transcriptPath) => {
    if (!transcriptPath) {
        return null;
    }
    let raw = '';
    try {
        raw = await readFile(transcriptPath, 'utf8');
    }
    catch (error) {
        if (error &&
            typeof error === 'object' &&
            'code' in error &&
            error.code === 'ENOENT') {
            return null;
        }
        throw error;
    }
    const lines = raw.split('\n').filter((line) => line.trim().length > 0);
    for (let index = lines.length - 1; index >= 0; index -= 1) {
        const entry = JSON.parse(lines[index]);
        if (entry.type !== 'assistant' || entry.message?.role !== 'assistant') {
            continue;
        }
        const text = extractTextFromContent(entry.message.content);
        if (text.length === 0) {
            continue;
        }
        return {
            uuid: entry.uuid ?? null,
            text,
            isComplete: entry.message.stop_reason !== null && entry.message.stop_reason !== undefined,
        };
    }
    return null;
};
const resolveLatestAssistantEntry = async (payloadSessionId, transcriptPath) => {
    const latestFromPayloadTranscript = await readLatestAssistantEntryFromTranscript(transcriptPath ?? null);
    if (latestFromPayloadTranscript?.text) {
        return latestFromPayloadTranscript;
    }
    for (const candidatePath of buildTranscriptPathCandidates(payloadSessionId)) {
        const latestCandidateEntry = await readLatestAssistantEntryFromTranscript(candidatePath);
        if (latestCandidateEntry?.text) {
            return latestCandidateEntry;
        }
    }
    const discoveredTranscriptPath = await findTranscriptPathBySessionId(payloadSessionId);
    return readLatestAssistantEntryFromTranscript(discoveredTranscriptPath);
};
export const readStableAssistantEntryForStop = async ({
    relaySessionId,
    payloadSessionId,
    transcriptPath,
    hookEventFilePath,
}) => {
    const lastTerminalTurnId = await findLastTerminalTurnId(
        hookEventFilePath,
        relaySessionId,
    );
    const maxAttempts = 10;
    let latestSeenEntry = null;
    for (let attempt = 0; attempt < maxAttempts; attempt += 1) {
        const latestEntry = await resolveLatestAssistantEntry(payloadSessionId, transcriptPath);
        if (latestEntry?.text) {
            latestSeenEntry = latestEntry;
            const latestTurnId = buildAssistantTurnId(payloadSessionId, latestEntry.uuid);
            if (latestEntry.isComplete && latestTurnId && latestTurnId !== lastTerminalTurnId) {
                return latestEntry;
            }
            if (latestEntry.isComplete && !latestTurnId) {
                return latestEntry;
            }
        }
        if (attempt < maxAttempts - 1) {
            await sleep(100);
        }
    }
    if (!latestSeenEntry?.text) {
        return null;
    }
    const latestTurnId = buildAssistantTurnId(payloadSessionId, latestSeenEntry.uuid);
    if (latestTurnId && latestTurnId === lastTerminalTurnId) {
        return null;
    }
    return latestSeenEntry;
};
export const readLastAssistantTextFromTranscript = async (transcriptPath) => {
    return (await readLatestAssistantEntryFromTranscript(transcriptPath))?.text ?? '';
};
export const buildProjectSessionLogPath = (cwd, sessionId) => {
    const encodedProjectPath = cwd.replace(/\//g, '-');
    return join(process.env.HOME ?? '', '.claude', 'projects', encodedProjectPath, `${sessionId}.jsonl`);
};
const resolveProjectRootForTranscript = () => {
    return process.env.SLACK_REMOTE_PROJECT_ROOT?.trim() || null;
};
const buildTranscriptPathCandidates = (sessionId) => {
    const candidates = [
        resolveProjectRootForTranscript(),
    ].filter((value, index, array) => Boolean(value) && array.indexOf(value) === index);
    return candidates.map((projectRoot) => buildProjectSessionLogPath(projectRoot, sessionId));
};
const findTranscriptPathBySessionId = async (sessionId) => {
    const projectsRoot = join(process.env.HOME ?? '', '.claude', 'projects');
    const visit = async (directoryPath) => {
        let entries;
        try {
            entries = (await readdir(directoryPath, { withFileTypes: true }));
        }
        catch (error) {
            if (error && typeof error === 'object' && 'code' in error && error.code === 'ENOENT') {
                return null;
            }
            throw error;
        }
        for (const entry of entries) {
            const entryPath = join(directoryPath, entry.name);
            if (entry.isDirectory()) {
                const found = await visit(entryPath);
                if (found) {
                    return found;
                }
                continue;
            }
            if (entry.isFile() && entry.name === `${sessionId}.jsonl`) {
                return entryPath;
            }
        }
        return null;
    };
    return visit(projectsRoot);
};
export const runStopHookFromStdin = async () => {
    const raw = await readStdin();
    const payload = JSON.parse(raw);
    const hookEventFilePath = process.env.SLACK_REMOTE_HOOK_EVENT_FILE;
    if (!hookEventFilePath) {
        return;
    }
    const relaySessionId = process.env.SLACK_REMOTE_SESSION_ID ?? payload.session_id;
    let text = '';
    let stopTurnKey = null;
    if (payload.hook_event_name === 'Notification') {
        text = payload.message?.trim() ?? '';
    }
    else if (payload.hook_event_name === 'PreToolUse') {
        text = payload.tool_name?.trim() ?? '';
    }
    else if (payload.hook_event_name === 'PostToolUse') {
        text = 'done';
    }
    else {
        const stableEntry = await readStableAssistantEntryForStop({
            relaySessionId,
            payloadSessionId: payload.session_id,
            transcriptPath: payload.transcript_path ?? null,
            hookEventFilePath,
        });
        text = stableEntry?.text ?? '';
        stopTurnKey = stableEntry?.uuid ?? null;
    }
    if (text.length === 0) {
        return;
    }
    await recordStopHookEvent({
        hookEventName: payload.hook_event_name,
        sessionId: relaySessionId,
        turnId: stopTurnKey
            ? `${payload.session_id}:${stopTurnKey}`
            : `${payload.session_id}:${Date.now()}`,
        hookEventFilePath,
        transcriptPath: payload.transcript_path ?? null,
        text,
    });
};
if (import.meta.url === `file://${process.argv[1]}`) {
    await runStopHookFromStdin();
}
