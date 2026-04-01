export async function onRequest(context) {
  const { params, env } = context;
  const shareId = params.id;
  
  if (!shareId) {
    return new Response('Share ID required', { status: 400 });
  }
  
  try {
    // Fetch data from R2
    const object = await env.R2_BUCKET.get(`shares/${shareId}.json`);
    
    if (!object) {
      return new Response('Share not found', { status: 404 });
    }
  
  const data = await object.json();
  
  // Track share view event
  await trackShareView(shareId, context.request);
  
    // Return the share.html template with injected data
    const url = new URL(context.request.url);
    const shareHtmlUrl = new URL('/share.html', url.origin);
    const html = await env.ASSETS.fetch(shareHtmlUrl);
    const template = await html.text();
    
    // Inject data into template
    // Escape </script> tags in JSON to prevent breaking the script tag
    const jsonData = JSON.stringify(data).replace(/<\/script>/gi, '<\\/script>');
    let finalHtml = template.replace(
      '// SHARE_DATA_INJECTION',
      `window.SHARE_DATA = ${jsonData};`
    );
    
    // Replace GA measurement ID
    finalHtml = finalHtml.replace(/G-XXXXXXXXXX/g, env.GA_MEASUREMENT_ID || 'G-XXXXXXXXXX');
    
    return new Response(finalHtml, {
      headers: { 'Content-Type': 'text/html' }
    });
  } catch (error) {
    console.error('Error in share function:', error);
    return new Response(`Error: ${error.message}`, { status: 500 });
  }
}

async function trackShareView(shareId, request) {
  // Server-side event tracking to Google Analytics
  const ua = request.headers.get('user-agent') || 'Unknown';
  const ip = request.headers.get('cf-connecting-ip') || 'Unknown';
  
  // Log view for internal analytics
  console.log(`Share viewed: ${shareId}, UA: ${ua}, IP: ${ip}`);
}