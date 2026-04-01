export async function onRequest(context) {
  const { env } = context;
  
  try {
    // Fetch gallery index from R2
    const object = await env.R2_BUCKET.get('gallery-index.json');
    
    if (!object) {
      // Return empty gallery if not found
      return new Response(JSON.stringify({ projects: [] }), {
        headers: { 
          'Content-Type': 'application/json',
          'Cache-Control': 'public, max-age=300' // Cache for 5 minutes
        }
      });
    }
    
    const data = await object.text();
    
    return new Response(data, {
      headers: { 
        'Content-Type': 'application/json',
        'Cache-Control': 'public, max-age=300' // Cache for 5 minutes
      }
    });
  } catch (error) {
    console.error('Failed to fetch gallery index:', error);
    return new Response(JSON.stringify({ error: 'Failed to load gallery' }), {
      status: 500,
      headers: { 'Content-Type': 'application/json' }
    });
  }
}
