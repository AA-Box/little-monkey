import { describe,expect,it } from 'vitest';
import { WORKFLOW_TOOL } from './tools';

describe('native workflow tool hierarchical argument contract',()=>{
  it('exposes exactly three finite, closed delegation levels',()=>{
    const root = (WORKFLOW_TOOL.function.parameters as {
      properties: Record<string,{items?:unknown}>;
    }).properties.tasks.items as {
      properties:Record<string,{items?:unknown}>;
      additionalProperties:boolean;
    };
    const child = root.properties.children.items as typeof root;
    const grandchild = child.properties.children.items as typeof root;
    expect(root.additionalProperties).toBe(false);
    expect(child.additionalProperties).toBe(false);
    expect(grandchild.additionalProperties).toBe(false);
    expect(grandchild.properties.children).toBeUndefined();
    expect(root.properties.peer).toBeDefined();
    expect(child.properties.isolation).toBeDefined();
  });
});
