import { SHARED } from "../../shared/value";
export default {
  async fetch(req: Request, env: any): Promise<Response> {
    const u = new URL(req.url);
    if (u.pathname === "/shared") { return new Response(SHARED); }
    if (u.pathname === "/write") { return env.API.fetch(new Request("http://api/write?src=web")); }
    const a = await (await env.API.fetch("http://api/")).text();
    const f = await env.FILES.get("a.txt");
    console.log("web saw", a);
    return new Response(`web <- ${a} | file=${f ? await f.text() : "none"} | secret=${env.STRIPE_KEY ? "leaked" : "none"}`);
  },
};
