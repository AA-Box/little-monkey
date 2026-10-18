/** Normalize a display name into a URL-friendly identifier. */
export function normalizeUserId(name) {
  if (typeof name !== 'string') {
    throw new TypeError('name must be a string');
  }
  return name.trim().toLowerCase().replaceAll(' ', '-');
}
