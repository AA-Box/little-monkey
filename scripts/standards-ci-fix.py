from pathlib import Path

p = Path('src/lib/agentLoop.ts')
s = p.read_text()

old = '''        const failure: VerifyFailure = {
          label: result.label,
          code: result.code,
          output,
          required: requiredSet.has(cmd.id),
          fixable: true,
        };
        if (firstFailure === null) firstFailure = failure;
        if (failure.required && firstRequiredFailure === null) firstRequiredFailure = failure;'''
new = '''        const required = requiredSet.has(cmd.id);
        const failure: VerifyFailure = {
          label: result.label,
          code: result.code,
          output,
          ...(required ? { required: true, fixable: true } : {}),
        };
        if (firstFailure === null) firstFailure = failure;
        if (required && firstRequiredFailure === null) firstRequiredFailure = failure;'''
if old not in s:
    raise SystemExit('normal verification failure target missing')
s = s.replace(old, new, 1)

old = '''      const failure: VerifyFailure = {
        label: cmd.label,
        code: null,
        output,
        required: requiredSet.has(cmd.id),
        fixable: true,
      };
      if (firstFailure === null) firstFailure = failure;
      if (failure.required && firstRequiredFailure === null) firstRequiredFailure = failure;'''
new = '''      const required = requiredSet.has(cmd.id);
      const failure: VerifyFailure = {
        label: cmd.label,
        code: null,
        output,
        ...(required ? { required: true, fixable: true } : {}),
      };
      if (firstFailure === null) firstFailure = failure;
      if (required && firstRequiredFailure === null) firstRequiredFailure = failure;'''
if old not in s:
    raise SystemExit('exception verification failure target missing')
p.write_text(s.replace(old, new, 1))
