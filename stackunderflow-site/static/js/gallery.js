// Gallery functionality for stackunderflow.dev
document.addEventListener('DOMContentLoaded', async () => {
  await loadGallery();
});

async function loadGallery() {
  const galleryGrid = document.getElementById('gallery-grid');
    
  try {
    // Fetch gallery index from the server
    const response = await fetch('/gallery-index.json');
    const data = response.ok ? await response.json() : { projects: [] };
        
    if (data.projects && data.projects.length > 0) {
      // Sort projects: featured first, then by created_at descending (newest first)
      const featured = data.projects.filter(p => p.featured);
      const nonFeatured = data.projects.filter(p => !p.featured);
      
      // Sort each group by date
      featured.sort((a, b) => new Date(b.created_at) - new Date(a.created_at));
      nonFeatured.sort((a, b) => new Date(b.created_at) - new Date(a.created_at));
      
      // Combine: featured first, then non-featured
      const sortedProjects = [...featured, ...nonFeatured];
      
      galleryGrid.innerHTML = sortedProjects.map(project => {
        // Format the created date
        const createdDate = new Date(project.created_at);
        const dateStr = createdDate.toLocaleDateString('en-US', {
          month: 'short',
          day: 'numeric',
          year: 'numeric'
        });
        
        // Determine the base URL based on environment
        const shareUrl = window.location.hostname === 'localhost' 
          ? `http://localhost:4001/share/${project.id}`
          : `/share/${project.id}`;
        
        return `
          <div class="gallery-item ${project.featured ? 'featured' : ''}" onclick="window.open('${shareUrl}', '_blank')">
              <div class="gallery-header">
                  <h3>${project.title}</h3>
                  <span class="date">${dateStr}</span>
              </div>
              ${project.featured ? '<div class="featured-indicator">⭐ Featured</div>' : ''}
              <div class="stats">
                  <span class="${project.includes_commands ? 'with-commands' : ''}" title="User commands${project.includes_commands ? ' (full commands available)' : ' (full commands unavailable)'}">🗣️ ${formatNumber(project.stats?.total_commands || 0)}</span>
                  <span title="Project duration">🗓️ ${project.stats?.duration_days || 0} days</span>
                  <span title="Total tokens used">🔤 ${formatNumber(project.stats?.total_tokens || 0)}</span>
                  ${project.stats?.total_cost ? `<span title="Cost if used API directly" class="${project.stats.total_cost > 10 ? 'high-cost' : ''}">💰 $${project.stats.total_cost.toFixed(2)}</span>` : ''}
                  <span title="Percentage of commands interrupted by user" class="${(project.stats?.interruption_rate || 0) > 20 ? 'high-interruption' : ''}">⚡ ${project.stats?.interruption_rate || 0}%</span>
                  <span title="Average assistant steps per user command">⛓️ ${(project.stats?.avg_steps_per_command || 0).toFixed(1)}</span>
              </div>
          </div>
        `;
      }).join('');
    } else {
      // Show empty state
      galleryGrid.innerHTML = `
        <div style="grid-column: 1 / -1; text-align: center; padding: 3rem;">
          <p style="color: #666; font-size: 1.1rem;">No public dashboards yet.</p>
          <p style="color: #999; margin-top: 0.5rem;">Share your Claude Code analytics to be the first!</p>
        </div>
      `;
    }
  } catch (error) {
    console.error('Failed to load gallery:', error);
    galleryGrid.innerHTML = '<p style="grid-column: 1 / -1; text-align: center;">Failed to load gallery. Please try again later.</p>';
  }
}

function formatNumber(num) {
  if (num >= 1_000_000) {
    return (num / 1_000_000).toFixed(1) + 'M';
  } else if (num >= 1_000) {
    return (num / 1_000).toFixed(1) + 'K';
  }
  return num.toString();
}