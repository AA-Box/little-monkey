import { ShieldCheck, X } from "lucide-react";
import { useSkillActivationApprovalStore } from "../../store/skillActivationApprovalStore";
import { Button } from "../ui";

export function SkillActivationApprovalModal() {
  const pending = useSkillActivationApprovalStore((state) => state.pending);
  const allowOnce = useSkillActivationApprovalStore((state) => state.allowOnce);
  const deny = useSkillActivationApprovalStore((state) => state.deny);

  if (!pending) return null;
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4 backdrop-blur-[2px]" role="dialog" aria-modal="true" aria-labelledby="skill-activation-approval-title">
      <div className="w-full max-w-md rounded-xl border border-border bg-background p-5 shadow-xl">
        <div className="flex items-start gap-3">
          <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-warning-soft text-warning"><ShieldCheck size={18} /></div>
          <div>
            <h2 id="skill-activation-approval-title" className="text-sm font-semibold text-foreground">Allow skill once?</h2>
            <p className="mt-1 text-xs text-muted">The model requested <span className="font-mono">/{pending.command}</span> ({pending.name}).</p>
            {pending.description && <p className="mt-2 text-xs text-muted">{pending.description}</p>}
          </div>
        </div>
        <div className="mt-5 flex justify-end gap-2">
          <Button type="button" variant="secondary" onClick={deny}><X size={14} />Deny</Button>
          <Button type="button" variant="primary" onClick={allowOnce} autoFocus>Allow once</Button>
        </div>
      </div>
    </div>
  );
}

export default SkillActivationApprovalModal;
