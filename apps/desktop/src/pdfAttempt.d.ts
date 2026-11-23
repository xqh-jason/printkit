export interface PdfAttempt {
  key: string;
  timedOut: boolean;
  outcome: null;
}

export function beginPdfAttempt(
  file: { name?: string; size?: number; lastModified?: number } | null,
  printer: string | undefined,
  copies: number,
  nonce: string,
): PdfAttempt;
export function markPdfTimeout(attempt: PdfAttempt | null): PdfAttempt | null;
export function markPdfTerminal(attempt: PdfAttempt | null, outcome: string): null;
export function settlePdfAttempt(): null;
export function refreshKey(attempt: PdfAttempt | null): string | null;
export function pdfLocked(busy: boolean, status: string | null | undefined, attempt: PdfAttempt | null): boolean;
