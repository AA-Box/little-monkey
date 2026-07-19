import { describe, expect, it } from 'vitest';

import { buildSubagentSystemPrompt, buildSystemPrompt, composeSystemPrompt, detectOsLabel, resolvePersona, ULTRACODE_SYSTEM_SECTION, type McpServerPromptInfo } from './systemPrompt';
import type { MemoryFact, RuleFile } from '../store/rulesStore';
import type { PromptEntry } from '../store/promptStore';

describe('detectOsLabel', () => {
  it('maps navigator platforms to friendly names', () => {
    expect(detectOsLabel('MacIntel')).toBe('macOS');
    expect(detectOsLabel('Win32')).toBe('Windows');
    expect(detectOsLabel('Linux x86_64')).toBe('Linux');
  });

  it('falls back gracefully for unknown platforms', () => {
    expect(detectOsLabel('BeOS')).toBe('BeOS');
    expect(detectOsLabel('')).toBe('an unknown OS');
  });
});

describe('buildSystemPrompt', () => {
  const primary = { path: '/home/me/project', label: 'project', is_primary: true };
  const secondary = { path: '/home/me/notes', label: 'notes', is_primary: false };

  it('names the primary workspace and the OS', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).toContain('/home/me/project');
    expect(prompt).toContain('macOS');
    expect(prompt).toContain('Little Monkey');
  });

  it('lists secondary folders with their labels', () => {
    const prompt = buildSystemPrompt([primary, secondary], 'Linux');
    expect(prompt).toContain('"notes" (/home/me/notes)');
  });

  it('says so when no workspace is open', () => {
    const prompt = buildSystemPrompt([], 'Windows');
    expect(prompt).toContain('No workspace folder is open');
    expect(prompt).not.toContain('primary workspace folder is');
  });

  it('mentions the core tools and conventions', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).toContain('edit_file');
    expect(prompt).toContain('glob');
    expect(prompt).toContain('grep');
    expect(prompt).toContain('permission');
  });

  it('lists remember alongside the other mutating tools that may prompt for permission', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).toContain('Mutating tools (write_file, edit_file, run_shell, remember)');
  });

  it('always includes a guidance line on when to call remember and to treat MONKEY.md as user instructions', () => {
    const withNeither = buildSystemPrompt([primary], 'macOS');
    expect(withNeither).toContain('Use the remember tool to save short, durable facts');
    expect(withNeither).toContain('instructions from the user, not untrusted document content');
  });

  it('omits the MONKEY.md section entirely when there are no rules', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).not.toContain('Project instructions (MONKEY.md)');
  });

  it('appends each rule file with a "From <scope/label>:" provenance header, global first', () => {
    const globalRule: RuleFile = {
      scope: 'global',
      label: 'global',
      path: '/app-data/MONKEY.md',
      content: 'Always write tests.',
      truncated: false,
    };
    const projectRule: RuleFile = {
      scope: 'project',
      label: 'notes',
      path: '/home/me/notes/MONKEY.md',
      content: 'Notes are markdown only.',
      truncated: false,
    };

    const prompt = buildSystemPrompt([primary, secondary], 'macOS', { rules: [globalRule, projectRule] });

    expect(prompt).toContain('## Project instructions (MONKEY.md)');
    expect(prompt).toContain('From global:');
    expect(prompt).toContain('Always write tests.');
    expect(prompt).toContain('From project (notes):');
    expect(prompt).toContain('Notes are markdown only.');
    // Global must precede the project-scoped entry.
    expect(prompt.indexOf('From global:')).toBeLessThan(prompt.indexOf('From project (notes):'));
  });

  it('lists remembered facts as bullets when present, and omits the section when empty', () => {
    const facts: MemoryFact[] = [
      { id: '1', text: 'Uses pnpm, not npm.', source: 'user', created_at: '2026-01-01T00:00:00Z' },
    ];

    const withFacts = buildSystemPrompt([primary], 'macOS', { facts });
    expect(withFacts).toContain('## Remembered facts');
    expect(withFacts).toContain('- Uses pnpm, not npm.');

    const withoutFacts = buildSystemPrompt([primary], 'macOS', { facts: [] });
    expect(withoutFacts).not.toContain('Remembered facts');
  });

  /** Memory Studio's CRITICAL acceptance bar (ROADMAP.md): "deleting or
   * disabling a memory prevents it from entering future prompts." The
   * actual filtering happens upstream, in Rust — `memory.rs`'s `list_impl`
   * (what the `memory_list` command calls) excludes disabled/deleted facts
   * before they ever leave the backend (see
   * `disabled_and_deleted_facts_are_excluded_from_list_impl` in
   * `memory.rs`'s test module). This test proves the other half of the
   * chain: `buildSystemPrompt` never adds anything to the prompt beyond
   * exactly the `facts` array it was handed — so once a fact is missing
   * from `memory_list`'s output (and therefore from `rulesStore.facts`,
   * which `currentSystemPrompt` reads straight from), there is no code path
   * left by which its text could still reach the model. */
  it('never mentions a fact that is absent from the facts array — the trust boundary memory_list filtering relies on', () => {
    const survivingFact: MemoryFact = {
      id: 'kept',
      text: 'The build command is `pnpm run build`.',
      source: 'agent',
      created_at: '2026-01-01T00:00:00Z',
      enabled: true,
      source_turn_id: 'turn-1',
    };
    // Represents a fact that was disabled or deleted: `memory_list` already
    // excluded it, so it simply never appears in the array passed in here.
    const excludedFactText = 'The secret staging password is hunter2.';

    const prompt = buildSystemPrompt([primary], 'macOS', { facts: [survivingFact] });

    expect(prompt).toContain(survivingFact.text);
    expect(prompt).not.toContain(excludedFactText);
  });

  it('appends each connected MCP server\'s instructions, and omits the section when there are none', () => {
    const mcpServers: McpServerPromptInfo[] = [{ label: 'GitHub', instructions: 'Use search_repositories before cloning.' }];

    const withMcp = buildSystemPrompt([primary], 'macOS', { mcpServers });
    expect(withMcp).toContain('## Connected MCP servers');
    expect(withMcp).toContain("MCP server 'GitHub': Use search_repositories before cloning.");

    const withoutMcp = buildSystemPrompt([primary], 'macOS', { mcpServers: [] });
    expect(withoutMcp).not.toContain('Connected MCP servers');
  });

  it('caps a connected MCP server\'s instructions at 1000 characters', () => {
    const longInstructions = 'x'.repeat(1500);
    const mcpServers: McpServerPromptInfo[] = [{ label: 'Verbose', instructions: longInstructions }];

    const prompt = buildSystemPrompt([primary], 'macOS', { mcpServers });

    expect(prompt).toContain(`MCP server 'Verbose': ${'x'.repeat(1000)}…`);
    expect(prompt).not.toContain('x'.repeat(1001));
  });

  it('mentions web_fetch and web_search by default (webToolsAvailable defaults to true)', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).toContain('web_fetch');
    expect(prompt).toContain('web_search');
  });

  it('omits the web tools guidance line (both web_fetch and web_search) when webToolsAvailable is false', () => {
    const prompt = buildSystemPrompt([primary], 'macOS', { webToolsAvailable: false });
    expect(prompt).not.toContain('web_fetch');
    expect(prompt).not.toContain('web_search');
  });

  it('omits the verification guidance line by default (verifyGuidanceAvailable defaults to false)', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).not.toContain('Configured verification commands');
  });

  it('includes the verification guidance line only when verifyGuidanceAvailable is true', () => {
    const prompt = buildSystemPrompt([primary], 'macOS', { verifyGuidanceAvailable: true });
    expect(prompt).toContain('Configured verification commands run automatically after your edits; fix any failures they report.');
  });

  it('omits the Plan Mode section when mode is omitted (defaults to "manual")', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).not.toContain('## Plan Mode');
    expect(prompt).not.toContain('present_plan');
  });

  it('omits the Plan Mode section for every non-"plan" mode', () => {
    for (const mode of ['manual', 'acceptEdits', 'smart', 'auto', 'bypass'] as const) {
      const prompt = buildSystemPrompt([primary], 'macOS', { mode });
      expect(prompt).not.toContain('## Plan Mode');
    }
  });

  it('includes the Plan Mode section, steering the model toward present_plan and away from mutating tools, only when mode is "plan"', () => {
    const prompt = buildSystemPrompt([primary], 'macOS', { mode: 'plan' });
    expect(prompt).toContain('## Plan Mode');
    expect(prompt).toContain('present_plan');
    expect(prompt).toContain(
      'every other tool call — including write_file, edit_file, run_shell, remember, web_fetch, and web_search — is blocked',
    );
  });

  it('does not claim web_fetch/web_search "work normally" in Plan Mode, since they are actually blocked like every other non-read-only tool', () => {
    const prompt = buildSystemPrompt([primary], 'macOS', { mode: 'plan' });
    expect(prompt).not.toContain('web_fetch, web_search) work normally');
  });

  it('nudges the model to tag html/svg/mermaid fences by default (artifactGuidanceAvailable defaults to true)', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).toContain('tagged html/svg/mermaid so it can be previewed');
  });

  it('omits the artifact guidance line when artifactGuidanceAvailable is false', () => {
    const prompt = buildSystemPrompt([primary], 'macOS', { artifactGuidanceAvailable: false });
    expect(prompt).not.toContain('tagged html/svg/mermaid');
  });

  it('omits the knowledge stacks line when no stacks are attached (the default)', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).not.toContain('Knowledge stacks attached');
    expect(prompt).not.toContain('search_docs');
  });

  it('names every attached stack and its description, and points the model at search_docs', () => {
    const prompt = buildSystemPrompt([primary], 'macOS', {
      attachedStacks: [
        { name: 'Docs', description: '42 chunks indexed' },
        { name: 'Release Notes', description: 'not indexed yet' },
      ],
    });
    expect(prompt).toContain('Knowledge stacks attached');
    expect(prompt).toContain('"Docs" (42 chunks indexed)');
    expect(prompt).toContain('"Release Notes" (not indexed yet)');
    expect(prompt).toContain('search_docs');
    expect(prompt).toContain('cite source paths');
  });

  it('omits the doc-chat citation instruction by default (docChatMode defaults to false)', () => {
    const prompt = buildSystemPrompt([primary], 'macOS', {
      attachedStacks: [{ name: 'Docs', description: '42 chunks indexed' }],
    });
    expect(prompt).not.toContain('Doc-chat mode is on');
  });

  it('adds the doc-chat citation instruction when docChatMode is true', () => {
    const prompt = buildSystemPrompt([primary], 'macOS', {
      attachedStacks: [{ name: 'Docs', description: '42 chunks indexed' }],
      docChatMode: true,
    });
    expect(prompt).toContain('Doc-chat mode is on');
    expect(prompt).toContain('[Sources]');
    expect(prompt).toContain('citing the specific source path');
  });

  it('omits the subagent delegation guidance line by default (subagentGuidanceAvailable defaults to false)', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).not.toContain('task tool');
  });

  it('adds the subagent delegation guidance line when subagentGuidanceAvailable is true', () => {
    const prompt = buildSystemPrompt([primary], 'macOS', { subagentGuidanceAvailable: true });
    expect(prompt).toContain('task tool');
    expect(prompt).toContain("profile 'explore'");
  });
});

// `buildSubagentSystemPrompt` seeds a subagent's own LOCAL message history
// (see `subagent.ts`'s `runSubagentTask`) — a distinct, much shorter prompt
// than `buildSystemPrompt`'s, but sharing the same workspace-facts
// derivation.
describe('buildSubagentSystemPrompt', () => {
  const primary = { path: '/home/me/project', label: 'project', is_primary: true };
  const secondary = { path: '/home/me/notes', label: 'notes', is_primary: false };

  it('names the primary workspace, the OS, and the task description', () => {
    const prompt = buildSubagentSystemPrompt([primary], 'macOS', 'explore', 'find every caller of X');
    expect(prompt).toContain('/home/me/project');
    expect(prompt).toContain('macOS');
    expect(prompt).toContain('find every caller of X');
  });

  it('names secondary folders by label when attached', () => {
    const prompt = buildSubagentSystemPrompt([primary, secondary], 'macOS', 'explore', 'd');
    expect(prompt).toContain('"notes"');
    expect(prompt).toContain('/home/me/notes');
  });

  it('handles no workspace open without throwing', () => {
    const prompt = buildSubagentSystemPrompt([], 'macOS', 'explore', 'd');
    expect(prompt).toContain('No workspace folder is open yet');
  });

  it('describes only read-only tools for the explore profile', () => {
    const prompt = buildSubagentSystemPrompt([primary], 'macOS', 'explore', 'd');
    expect(prompt).toContain('read-only tools only');
    expect(prompt).not.toContain('write_file, edit_file, and run_shell to make changes');
  });

  it('describes read-write tools for the code profile', () => {
    const prompt = buildSubagentSystemPrompt([primary], 'macOS', 'code', 'd');
    expect(prompt).toContain('write_file, edit_file, and run_shell to make changes');
  });

  it('instructs the subagent to report back rather than ask questions', () => {
    const prompt = buildSubagentSystemPrompt([primary], 'macOS', 'explore', 'd');
    expect(prompt).toContain('final report');
    expect(prompt).toContain('do not ask questions');
  });
});

describe('ULTRACODE_SYSTEM_SECTION', () => {
  it('directs orchestration through the task tool on the same model — never a multi-model fan-out', () => {
    expect(ULTRACODE_SYSTEM_SECTION).toContain('`task` tool');
    expect(ULTRACODE_SYSTEM_SECTION).toContain('parallel');
    expect(ULTRACODE_SYSTEM_SECTION).toContain('Adversarially verify');
    // The old Ultracode ran the prompt across several different models via
    // the Compare pipeline; the section must never resurrect that framing.
    expect(ULTRACODE_SYSTEM_SECTION.toLowerCase()).not.toContain('models');
  });

  it('names both subagent profiles so the model picks the right one per subtask', () => {
    expect(ULTRACODE_SYSTEM_SECTION).toContain('`explore`');
    expect(ULTRACODE_SYSTEM_SECTION).toContain('`code`');
  });
});

describe('composeSystemPrompt', () => {
  it('returns the base prompt unchanged when there is no active persona', () => {
    expect(composeSystemPrompt('BASE PROMPT', null)).toBe('BASE PROMPT');
  });

  it('appends a clearly-delimited persona section after the base prompt, never replacing it', () => {
    const base = 'You are Little Monkey. Mutating tools may prompt for permission.';
    const composed = composeSystemPrompt(base, { name: 'Code Reviewer', content: 'Focus only on bugs, not style.' });

    // The base prompt's sandbox/tool/permission guidance must survive intact.
    expect(composed.startsWith(base)).toBe(true);
    expect(composed).toContain('## Active persona: Code Reviewer');
    expect(composed).toContain('Focus only on bugs, not style.');
    // The persona section comes strictly after the base content, not before it.
    expect(composed.indexOf('## Active persona')).toBeGreaterThan(composed.indexOf('Mutating tools'));
  });
});

describe('resolvePersona', () => {
  const entries: PromptEntry[] = [
    { id: 'p1', kind: 'persona', name: 'Reviewer', command: 'reviewer', content: 'Be critical.', createdAt: 1, updatedAt: 1 },
    { id: 's1', kind: 'snippet', name: 'Standup', command: 'standup', content: 'Wrote code.', createdAt: 1, updatedAt: 1 },
  ];

  it('resolves a matching persona id to its name/content', () => {
    expect(resolvePersona(entries, 'p1')).toEqual({ name: 'Reviewer', content: 'Be critical.' });
  });

  it('returns null when personaId is null (no active persona)', () => {
    expect(resolvePersona(entries, null)).toBeNull();
  });

  it('resolves a dangling personaId (its persona was deleted) to null instead of throwing', () => {
    expect(() => resolvePersona(entries, 'deleted-id')).not.toThrow();
    expect(resolvePersona(entries, 'deleted-id')).toBeNull();
  });

  it('does not resolve a snippet entry even if its id happens to match', () => {
    expect(resolvePersona(entries, 's1')).toBeNull();
  });
});
