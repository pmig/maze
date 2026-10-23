import type { APIRoute } from "astro";
import { croppedIconSvg } from "../lib/icon";

export const GET: APIRoute = () =>
  new Response(croppedIconSvg(), { headers: { "Content-Type": "image/svg+xml" } });
