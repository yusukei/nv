export default {
  async fetch(req: Request): Promise<Response> {
    return new Response(`plain worker ${new URL(req.url).pathname}`);
  },
};
