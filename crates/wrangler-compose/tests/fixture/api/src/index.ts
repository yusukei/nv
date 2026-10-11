import { DurableObject } from "cloudflare:workers";
export class Counter extends DurableObject {
  async inc(): Promise<number> { const n = ((await this.ctx.storage.get<number>("n")) ?? 0) + 1; await this.ctx.storage.put("n", n); return n; }
}
export default {
  async fetch(req: Request, env: any): Promise<Response> {
    const u = new URL(req.url);
    if (u.pathname === "/write") { await env.DB.prepare("INSERT INTO items(src) VALUES (?)").bind(u.searchParams.get("src") ?? "api").run(); return new Response("ok"); }
    if (u.pathname === "/upload") { await env.UPLOADS.put("up/" + (u.searchParams.get("k") ?? "x"), "data"); return new Response("uploaded"); }
    if (u.pathname === "/remove") { await env.UPLOADS.delete("up/" + (u.searchParams.get("k") ?? "x")); return new Response("removed"); }
    if (u.pathname === "/count") { const r = await env.DB.prepare("SELECT count(*) n FROM items").first(); return Response.json(r); }
    await env.FILES.put("a.txt", "from-api");
    await env.CACHE.put("k", "v");
    await env.JOBS.send({ at: Date.now() });
    const n = await env.COUNTER.get(env.COUNTER.idFromName("x")).inc();
    console.log("api handled", u.pathname);
    return new Response(`api ${env.GREETING} do=${n} secret=${env.STRIPE_KEY ? "yes" : "no"}`);
  },
  async queue(batch: MessageBatch, env: any) { for (const m of batch.messages) console.log("msg", JSON.stringify(m.body)); await env.CACHE.put("consumed", String(batch.messages.length)); },
};
