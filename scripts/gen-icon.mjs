// Generates a 1024x1024 source PNG for `tauri icon`.
// Run: node scripts/gen-icon.mjs  →  icon-src.png
import sharp from "sharp";

const svg = `
<svg width="1024" height="1024" viewBox="0 0 1024 1024" xmlns="http://www.w3.org/2000/svg">
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#11271d"/>
      <stop offset="1" stop-color="#05100a"/>
    </linearGradient>
    <radialGradient id="glow" cx="0.5" cy="0.16" r="0.75">
      <stop offset="0" stop-color="#11ff99" stop-opacity="0.30"/>
      <stop offset="0.62" stop-color="#11ff99" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="leaf" x1="0.1" y1="0" x2="0.9" y2="1">
      <stop offset="0" stop-color="#46ffb9"/>
      <stop offset="1" stop-color="#0bd17e"/>
    </linearGradient>
  </defs>

  <rect width="1024" height="1024" rx="232" fill="url(#bg)"/>
  <rect width="1024" height="1024" rx="232" fill="url(#glow)"/>
  <rect x="6" y="6" width="1012" height="1012" rx="228" fill="none"
        stroke="#ffffff" stroke-opacity="0.06" stroke-width="2"/>

  <!-- stem -->
  <path d="M512 792 C 512 660 512 560 512 452"
        stroke="#0bd17e" stroke-width="40" stroke-linecap="round" fill="none"/>

  <!-- left leaf -->
  <path d="M512 600 C 396 596 318 520 300 414 C 430 410 506 480 512 600 Z"
        fill="url(#leaf)"/>
  <!-- right leaf -->
  <path d="M512 524 C 628 520 724 444 742 332 C 600 326 520 404 512 524 Z"
        fill="url(#leaf)"/>
  <!-- leaf veins -->
  <path d="M512 590 C 450 560 400 500 360 440" stroke="#06140d" stroke-opacity="0.35" stroke-width="10" fill="none" stroke-linecap="round"/>
  <path d="M512 516 C 580 486 650 432 700 372" stroke="#06140d" stroke-opacity="0.35" stroke-width="10" fill="none" stroke-linecap="round"/>
</svg>`;

await sharp(Buffer.from(svg)).resize(1024, 1024).png().toFile("icon-src.png");
console.log("wrote icon-src.png");
