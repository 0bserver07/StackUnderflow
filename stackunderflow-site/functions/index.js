export async function onRequest(context) {
  const { request, env } = context;
  
  // Fetch the index.html from static assets
  const response = await env.ASSETS.fetch(request);
  
  // Check if it's an HTML response
  const contentType = response.headers.get('content-type');
  if (!contentType || !contentType.includes('text/html')) {
    return response;
  }
  
  // Read the HTML content
  const html = await response.text();
  
  // Replace GA measurement ID if available
  let modifiedHtml = html;
  if (env.GA_MEASUREMENT_ID) {
    modifiedHtml = html.replace(/G-XXXXXXXXXX/g, env.GA_MEASUREMENT_ID);
  }
  
  // Return the modified HTML
  return new Response(modifiedHtml, {
    status: response.status,
    headers: response.headers
  });
}