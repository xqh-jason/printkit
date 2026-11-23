export type BinaryInput = Uint8Array | ArrayBuffer;

/**
 * Base64 of the exact bytes. `btoa` only accepts a binary string, so the input
 * is fed in chunks: spreading a multi-megabyte array into one call overflows the
 * argument limit of every engine.
 */
export function toBase64(input: BinaryInput): string {
  const bytes = input instanceof Uint8Array ? input : new Uint8Array(input);
  const chunk = 0x8000;
  let binary = "";
  for (let offset = 0; offset < bytes.length; offset += chunk) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + chunk));
  }
  return btoa(binary);
}
