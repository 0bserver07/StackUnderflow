export async function onRequestPost(context) {
  const { request, env } = context;
  
  try {
    console.log('Received share upload request');
    
    // Parse request body
    const body = await request.json();
    console.log('Request body size:', JSON.stringify(body).length);
    
    const { share_id, data, is_public } = body;
    
    if (!share_id || !data) {
      console.error('Missing required fields:', { share_id: !!share_id, data: !!data });
      return new Response(JSON.stringify({ error: 'Missing required fields' }), {
        status: 400,
        headers: { 'Content-Type': 'application/json' }
      });
    }
    
    console.log(`Saving share ${share_id} to R2, public: ${is_public}`);
    
    // Save share data to R2
    const shareKey = `shares/${share_id}.json`;
    
    try {
      const result = await env.R2_BUCKET.put(shareKey, JSON.stringify(data, null, 2), {
        httpMetadata: {
          contentType: 'application/json',
        },
        customMetadata: {
          'is-public': is_public ? 'true' : 'false'
        }
      });
      
      console.log('R2 put result:', result);
      console.log('Share saved to R2 successfully');
    } catch (r2Error) {
      console.error('R2 put failed:', r2Error);
      throw new Error(`Failed to save to R2: ${r2Error.message}`);
    }
    
    // Log share creation
    await logShareCreation(env, share_id, data, request);
    
    // If it's a public share, update the gallery
    if (is_public) {
      console.log('Updating gallery index');
      await updateGalleryIndex(env, share_id, data);
    }
    
    return new Response(JSON.stringify({ 
      success: true, 
      share_id: share_id,
      url: `https://stackunderflow.dev/share/${share_id}`
    }), {
      headers: { 'Content-Type': 'application/json' }
    });
    
  } catch (error) {
    console.error('Error creating share:', error.message, error.stack);
    return new Response(JSON.stringify({ error: 'Internal server error', details: error.message }), {
      status: 500,
      headers: { 'Content-Type': 'application/json' }
    });
  }
}

async function logShareCreation(env, shareId, data, request) {
  try {
    const logEntry = {
      id: shareId,
      created_at: data.created_at || new Date().toISOString(),
      is_public: data.is_public || false,
      project_name: data.project_name || 'Unknown',
      include_commands: (data.user_commands && data.user_commands.length > 0) || false,
      ip_hash: await hashIP(request.headers.get('cf-connecting-ip') || 'unknown'),
      user_agent: request.headers.get('user-agent') || 'unknown'
    };
    
    // Get current log
    const logKey = `share-logs/${new Date().toISOString().split('T')[0]}.jsonl`;
    let existingLog = '';
    
    try {
      const object = await env.R2_BUCKET.get(logKey);
      if (object) {
        existingLog = await object.text();
      }
    } catch (e) {
      // Log doesn't exist yet
    }
    
    // Append new entry
    const newLog = existingLog + JSON.stringify(logEntry) + '\n';
    
    // Upload updated log
    await env.R2_BUCKET.put(logKey, newLog, {
      httpMetadata: { contentType: 'text/plain' }
    });
    
  } catch (error) {
    console.error('Failed to log share creation:', error);
    // Don't fail the share creation if logging fails
  }
}

async function hashIP(ip) {
  const encoder = new TextEncoder();
  const data = encoder.encode(ip);
  const hashBuffer = await crypto.subtle.digest('SHA-256', data);
  const hashArray = Array.from(new Uint8Array(hashBuffer));
  return hashArray.map(b => b.toString(16).padStart(2, '0')).join('');
}

async function updateGalleryIndex(env, shareId, data) {
  try {
    // Get current gallery index
    let gallery = { projects: [] };
    
    try {
      const object = await env.R2_BUCKET.get('gallery-index.json');
      if (object) {
        gallery = await object.json();
      }
    } catch (e) {
      console.log('Gallery index not found, creating new one');
    }
    
    // Prepare gallery entry
    const stats = data.statistics || {};
    const totalTokens = stats.overview?.total_tokens || {};
    const totalTokenCount = (totalTokens.input || 0) + (totalTokens.output || 0);
    
    // Calculate duration
    let durationDays = 1;
    if (data.created_at && stats.overview?.last_activity) {
      const created = new Date(data.created_at);
      const lastActivity = new Date(stats.overview.last_activity);
      durationDays = Math.max(1, Math.floor((lastActivity - created) / (1000 * 60 * 60 * 24)));
    }
    
    const galleryEntry = {
      id: shareId,
      title: data.title || 'Untitled',
      description: data.description || '',
      project_name: data.project_name || 'Unknown Project',
      created_at: data.created_at || new Date().toISOString(),
      includes_commands: (data.user_commands && data.user_commands.length > 0) || false,
      stats: {
        total_commands: stats.user_interactions?.user_commands_analyzed || 0,
        total_tokens: totalTokenCount,
        duration_days: durationDays,
        total_cost: stats.overview?.total_cost || 0,
        interruption_rate: stats.user_interactions?.interruption_rate || 0,
        avg_steps_per_command: stats.user_interactions?.avg_steps_per_command || 0,
      },
    };
    
    // Add new entry to gallery
    gallery.projects.push(galleryEntry);
    
    // Upload updated gallery index
    await env.R2_BUCKET.put('gallery-index.json', JSON.stringify(gallery, null, 2), {
      httpMetadata: {
        contentType: 'application/json',
      }
    });
    
    console.log(`Updated gallery index with share: ${shareId}`);
    
  } catch (error) {
    console.error('Failed to update gallery:', error);
    // Don't fail the share creation if gallery update fails
  }
}
