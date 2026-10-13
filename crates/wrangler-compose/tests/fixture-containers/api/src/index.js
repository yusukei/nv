import { DurableObject } from "cloudflare:workers";

// Ask the container on port 8080, starting it first if needed.
async function ask(ctx, start, path) {
  const c = ctx.container;
  if (!c.running) start(c);
  for (let i = 0; i < 100; i++) {
    try {
      const r = await c.getTcpPort(8080).fetch("http://container" + path);
      return new Response((await r.text()).trim());
    } catch {
      await new Promise((r) => setTimeout(r, 300));
    }
  }
  return new Response("the container did not answer", { status: 503 });
}

export class Pinger extends DurableObject {
  fetch() {
    return ask(this.ctx, (c) => c.start(), "/index.html");
  }
}

export class Echo extends DurableObject {
  fetch() {
    return ask(
      this.ctx,
      (c) => c.start({ entrypoint: ["httpd", "-f", "-p", "8080", "-h", "/etc"], enableInternet: false }),
      "/hostname",
    );
  }
}

export default {
  fetch(req, env) {
    const path = new URL(req.url).pathname;
    if (path === "/ping") return env.PINGER.getByName("one").fetch(req);
    if (path === "/echo") return env.ECHO.getByName("one").fetch(req);
    return new Response("api");
  },
};
