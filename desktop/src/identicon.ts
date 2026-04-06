/**
 * Deterministic identicon generator — creates a unique symmetric pattern
 * from a hex fingerprint string, similar to MetaMask's Jazzicon / Ethereum blockies.
 *
 * Returns an SVG data URL that can be used as an <img> src.
 */

function hashBytes(hex: string): number[] {
  const clean = hex.replace(/[^0-9a-fA-F]/g, "");
  const bytes: number[] = [];
  for (let i = 0; i < clean.length; i += 2) {
    bytes.push(parseInt(clean.substring(i, i + 2), 16));
  }
  // Pad to at least 16 bytes
  while (bytes.length < 16) bytes.push(0);
  return bytes;
}

function hslToRgb(h: number, s: number, l: number): [number, number, number] {
  s /= 100;
  l /= 100;
  const k = (n: number) => (n + h / 30) % 12;
  const a = s * Math.min(l, 1 - l);
  const f = (n: number) =>
    l - a * Math.max(-1, Math.min(k(n) - 3, Math.min(9 - k(n), 1)));
  return [
    Math.round(f(0) * 255),
    Math.round(f(8) * 255),
    Math.round(f(4) * 255),
  ];
}

export function generateIdenticon(
  fingerprint: string,
  size: number = 36
): string {
  const bytes = hashBytes(fingerprint);

  // Derive colors from first bytes
  const hue1 = (bytes[0] * 360) / 256;
  const hue2 = ((bytes[1] * 360) / 256 + 120) % 360;
  const [r1, g1, b1] = hslToRgb(hue1, 65, 35); // dark bg
  const [r2, g2, b2] = hslToRgb(hue2, 70, 55); // bright fg

  const bg = `rgb(${r1},${g1},${b1})`;
  const fg = `rgb(${r2},${g2},${b2})`;

  // 5x5 grid, left-right symmetric (only need 3 columns)
  const grid: boolean[][] = [];
  for (let y = 0; y < 5; y++) {
    const row: boolean[] = [];
    for (let x = 0; x < 3; x++) {
      const byteIdx = 2 + y * 3 + x;
      row.push(bytes[byteIdx % bytes.length] > 128);
    }
    // Mirror: col 3 = col 1, col 4 = col 0
    grid.push([row[0], row[1], row[2], row[1], row[0]]);
  }

  // Render SVG
  const cellSize = size / 5;
  const r = size * 0.12; // border radius
  let rects = "";
  for (let y = 0; y < 5; y++) {
    for (let x = 0; x < 5; x++) {
      if (grid[y][x]) {
        rects += `<rect x="${x * cellSize}" y="${y * cellSize}" width="${cellSize}" height="${cellSize}" fill="${fg}"/>`;
      }
    }
  }

  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 ${size} ${size}">
    <rect width="${size}" height="${size}" rx="${r}" fill="${bg}"/>
    ${rects}
  </svg>`;

  return `data:image/svg+xml,${encodeURIComponent(svg)}`;
}

/**
 * Create an <img> element with the identicon.
 * Click copies the fingerprint to clipboard.
 */
export function createIdenticonEl(
  fingerprint: string,
  size: number = 36,
  clickToCopy: boolean = true
): HTMLImageElement {
  const img = document.createElement("img");
  img.src = generateIdenticon(fingerprint, size);
  img.width = size;
  img.height = size;
  img.style.borderRadius = `${size * 0.12}px`;
  img.style.cursor = clickToCopy ? "pointer" : "default";
  img.title = fingerprint;

  if (clickToCopy && fingerprint) {
    img.addEventListener("click", (e) => {
      e.stopPropagation();
      navigator.clipboard.writeText(fingerprint).then(() => {
        img.style.outline = "2px solid #4ade80";
        setTimeout(() => {
          img.style.outline = "";
        }, 600);
      });
    });
  }

  return img;
}
