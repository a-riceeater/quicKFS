export const PAIRING_CODE_LENGTH = 27;
export const PAIRING_CODE_GROUPS = [4, 4, 4, 4, 4, 4, 3] as const;

const GROUPED_PAIRING_CODE_LENGTH = PAIRING_CODE_LENGTH + PAIRING_CODE_GROUPS.length - 1;
const BASE64_URL_CHARACTER = /^[A-Za-z0-9_-]$/;

function removeFormattingSeparators(value: string): string {
  if (value.length !== GROUPED_PAIRING_CODE_LENGTH) {
    return value;
  }

  let cursor = 0;
  const groups: string[] = [];
  for (const size of PAIRING_CODE_GROUPS) {
    groups.push(value.slice(cursor, cursor + size));
    cursor += size;
    if (cursor < value.length) {
      if (value[cursor] !== "-") {
        return value;
      }
      cursor += 1;
    }
  }
  return groups.join("");
}

export function normalizePairingCode(value: string): string {
  const withoutWhitespace = value.replace(/\s/g, "");
  const ungrouped = removeFormattingSeparators(withoutWhitespace);
  return Array.from(ungrouped)
    .filter((character) => BASE64_URL_CHARACTER.test(character))
    .join("")
    .slice(0, PAIRING_CODE_LENGTH);
}

export function splitPairingCode(value: string): string[][] {
  const characters = Array.from(value.padEnd(PAIRING_CODE_LENGTH, " "));
  let cursor = 0;
  return PAIRING_CODE_GROUPS.map((size) => {
    const group = characters.slice(cursor, cursor + size);
    cursor += size;
    return group;
  });
}
