import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { commands } from "@/bindings";

type ApprovedShortcuts = Partial<Record<string, string>>;

/** Desktop assignments are runtime labels, separate from shortcut preferences. */
export const usePortalShortcuts = (enabled: boolean): ApprovedShortcuts => {
  const [approved, setApproved] = useState<ApprovedShortcuts>({});
  useEffect(() => {
    setApproved({});
    if (!enabled) return;
    let disposed = false;
    let receivedUpdate = false;
    // Subscribe before taking the snapshot so a live rebind cannot be missed.
    const subscription = listen<ApprovedShortcuts>(
      "portal-shortcuts-changed",
      (event) => {
        if (disposed) return;
        receivedUpdate = true;
        setApproved(event.payload);
      },
    );
    subscription
      .then(async () => {
        const result = await commands.getPortalShortcuts();
        if (!disposed && !receivedUpdate && result.status === "ok")
          setApproved(result.data);
      })
      .catch(console.error);
    return () => {
      disposed = true;
      subscription.then((unlisten) => unlisten()).catch(console.error);
    };
  }, [enabled]);
  return approved;
};
