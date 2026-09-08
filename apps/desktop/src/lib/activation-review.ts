import type { DeepReadonly } from "vue";
import type { ActivationDisplay, ActivationStatus } from "./bridge";
import { policyFields } from "./policy-review";

export function activationSections(display: DeepReadonly<ActivationDisplay>) {
  return [
    { title: "Registry identity", fields: policyFields(display.route) },
    { title: "Policy limits and approval", fields: policyFields(display.policy) },
    { title: "Pilot authorization and accounting", fields: policyFields(display.pilot) },
    { title: "Venue wallet approval", fields: policyFields(display.wallet_approval) },
    { title: "Account observation", fields: policyFields(display.account) },
  ];
}

/** Identical cached polls retain consent; changed evidence or scope does not. */
export function activationConsentKey(status: DeepReadonly<ActivationStatus> | null, context: string, blocked: boolean): string {
  return JSON.stringify([context, blocked, status?.owner_id, status?.phase, status?.review]);
}
