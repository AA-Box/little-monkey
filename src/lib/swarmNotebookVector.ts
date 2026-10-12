/** Operator-approved local notebook claims can be explicitly published to
 * the separate authenticated vector memory subsystem. This merely prepares
 * a bounded inert payload: embedding, revisions and remote permissions are
 * enforced by the existing Knowledge 2.0 / peers native tools.
 */
import type { SwarmNotebookNote } from './swarmNotebook';

export interface ApprovedVectorClaim {
  id: string;
  text: string;
  approvedVersion: number;
}
export function approvedNotebookVectorClaim(note: SwarmNotebookNote): ApprovedVectorClaim {
  const active=note.revisions.find(item =>
    item.version===note.activeVersion && item.status==='approved');
  if (!active) throw new Error('Only an explicitly approved notebook revision can be exported.');
  if (!active.evidence.length) throw new Error('Reviewed claim must cite evidence.');
  const document='Claim identifier: '+note.id+
    '\nEvidence locations (historical, verify current source):\n'+active.evidence.join('\n');
  if (new TextEncoder().encode(document).byteLength>2048 || /[\u0000-\u0008\u000B\u000C\u000E-\u001F]/.test(document)) {
    throw new Error('Approved claim exceeds the native bounded vector memory limit.');
  }
  let hash=0x811c9dc5;
  for (const char of note.project+'\u0000'+note.id) {
    hash=Math.imul(hash^char.charCodeAt(0),0x01000193);
  }
  const slug=note.id.toLowerCase().replace(/[^a-z0-9._-]+/g,'-').replace(/^[^a-z0-9]+/,'').slice(0,53)||'claim';
  return {
    id:'review-'+slug+'-'+(hash>>>0).toString(16).padStart(8,'0'),
    text:document,
    approvedVersion:active.version,
  };
}
