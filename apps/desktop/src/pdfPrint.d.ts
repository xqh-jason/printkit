export function pdfRequestId(
  file: { name?: string; size?: number; lastModified?: number } | null,
  printer: string | undefined,
  copies: number,
  attempt?: string,
): string;
export function pdfAttemptNonce(): string;
export function isJobActive(status: string | null | undefined): boolean;
export function isJobTerminal(status: string | null | undefined): boolean;
export function jobOutcome(reply: { status?: string } | null, deadline: number, now?: number): "done" | "failed" | "cancelled" | "timeout" | "active";
export function canRunPdf(file: unknown, busy: boolean, active: boolean): boolean;
