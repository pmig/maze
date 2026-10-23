import type { APIRoute } from "astro";
import sharp from "sharp";
import { croppedIconSvg } from "../lib/icon";

// iOS rounds the corners itself, so the icon is flattened onto its own top color.
export const GET: APIRoute = async () => {
  const png = await sharp(Buffer.from(croppedIconSvg()), { density: 72 })
    .resize(180, 180)
    .flatten({ background: "#6E6BF8" })
    .png({ compressionLevel: 9 })
    .toBuffer();
  return new Response(new Uint8Array(png), { headers: { "Content-Type": "image/png" } });
};
