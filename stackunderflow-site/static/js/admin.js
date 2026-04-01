/**
 * Admin dashboard functionality
 */

let allProjects = [];
let currentFilter = 'all';
let searchTerm = '';

// Initialize admin dashboard
document.addEventListener('DOMContentLoaded', async () => {
  await loadAdminInfo();
  await loadProjects();
  await loadShareStats();
    
  // Set up event listeners
  document.getElementById('search-input').addEventListener('input', (e) => {
    searchTerm = e.target.value.toLowerCase();
    renderProjects();
  });
    
  document.getElementById('filter-select').addEventListener('change', (e) => {
    currentFilter = e.target.value;
    renderProjects();
  });
});

async function loadAdminInfo() {
  try {
    const response = await fetch('/admin/api/me');
    if (!response.ok) {
      window.location.href = '/admin/login';
      return;
    }
        
    const admin = await response.json();
    document.getElementById('admin-email').textContent = admin.email;
    if (admin.picture) {
      document.getElementById('admin-avatar').src = admin.picture;
    } else {
      // Hide avatar if no picture (unlikely with Google OAuth)
      document.getElementById('admin-avatar').style.display = 'none';
    }
  } catch (error) {
    console.error('Failed to load admin info:', error);
    window.location.href = '/admin/login';
  }
}

async function loadProjects() {
  try {
    const response = await fetch('/admin/api/gallery');
    if (!response.ok) {throw new Error('Failed to load gallery');}
        
    const data = await response.json();
    allProjects = data.projects || [];
        
    // Update stats
    document.getElementById('total-count').textContent = allProjects.length;
    document.getElementById('featured-count').textContent = 
            allProjects.filter(p => p.featured).length;
        
    renderProjects();
  } catch (error) {
    console.error('Failed to load projects:', error);
    document.getElementById('projects-grid').innerHTML = 
            '<div class="error">Failed to load projects. Please try again.</div>';
  }
}

async function loadShareStats() {
  try {
    const response = await fetch('/admin/api/share-stats');
    if (!response.ok) {throw new Error('Failed to load share stats');}
        
    const stats = await response.json();
        
    // Update share statistics
    document.getElementById('total-shares').textContent = stats.total || 0;
    document.getElementById('public-shares').textContent = stats.public || 0;
    document.getElementById('private-shares').textContent = stats.private || 0;
    document.getElementById('shares-with-commands').textContent = stats.with_commands || 0;
    
    // Update active/deleted breakdowns
    document.getElementById('total-active').textContent = stats.total_active || 0;
    document.getElementById('total-deleted').textContent = stats.total_deleted || 0;
    document.getElementById('public-active').textContent = stats.public_active || 0;
    document.getElementById('public-deleted').textContent = stats.public_deleted || 0;
    document.getElementById('private-active').textContent = stats.private_active || 0;
    document.getElementById('private-deleted').textContent = stats.private_deleted || 0;
    document.getElementById('commands-active').textContent = stats.with_commands_active || 0;
    document.getElementById('commands-deleted').textContent = stats.with_commands_deleted || 0;
        
  } catch (error) {
    console.error('Failed to load share stats:', error);
    // Don't show error in UI, just leave the zeros
  }
}

function renderProjects() {
  const grid = document.getElementById('projects-grid');
    
  // Filter projects
  let filtered = allProjects;
    
  // Apply filter
  if (currentFilter === 'featured') {
    filtered = filtered.filter(p => p.featured);
  } else if (currentFilter === 'non-featured') {
    filtered = filtered.filter(p => !p.featured);
  } else if (currentFilter === 'with-commands') {
    filtered = filtered.filter(p => p.includes_commands);
  } else if (currentFilter === 'without-commands') {
    filtered = filtered.filter(p => !p.includes_commands);
  }
    
  // Apply search
  if (searchTerm) {
    filtered = filtered.filter(p => {
      const projectName = p.project_name || 'Unknown Project';
      const id = p.id || '';
      return projectName.toLowerCase().includes(searchTerm) || 
                   id.toLowerCase().includes(searchTerm);
    });
  }
    
  if (filtered.length === 0) {
    grid.innerHTML = '<div class="no-results">No projects found</div>';
    return;
  }
    
  grid.innerHTML = filtered.map(project => `
        <div class="admin-project-card ${project.featured ? 'featured' : ''}">
            <div class="project-header">
                <h3>${escapeHtml(project.project_name || 'Unknown Project')}</h3>
                ${project.featured ? '<span class="featured-badge">⭐ Featured</span>' : ''}
            </div>
            
            <div class="project-info">
                <div class="info-row">
                    <span class="label">ID:</span>
                    <code>${project.id}</code>
                </div>
                <div class="info-row">
                    <span class="label">Created:</span>
                    <span>${formatDate(project.created_at)}</span>
                </div>
                <div class="info-row ${project.includes_commands ? 'with-commands' : ''}">
                    <span class="label">Commands:</span>
                    <span>${project.stats?.total_commands || 0}${project.includes_commands ? ' 💬' : ''}</span>
                </div>
                <div class="info-row">
                    <span class="label">Tokens:</span>
                    <span>${formatNumber(project.stats?.total_tokens || 0)}</span>
                </div>
                ${project.featured ? `
                    <div class="info-row">
                        <span class="label">Featured by:</span>
                        <span>${escapeHtml(project.featured_by || 'Unknown')}</span>
                    </div>
                ` : ''}
            </div>
            
            <div class="project-actions">
                <a href="${project.share_url}" target="_blank" class="btn btn-view">
                    View Share
                </a>
                ${project.featured ? `
                    <button onclick="unfeatureProject('${project.id}')" class="btn btn-unfeature">
                        Remove Feature
                    </button>
                ` : `
                    <button onclick="featureProject('${project.id}')" class="btn btn-feature">
                        ⭐ Feature
                    </button>
                `}
                <button onclick="removeProject('${project.id}')" class="btn btn-remove">
                    🗑️ Remove
                </button>
            </div>
        </div>
    `).join('');
}

async function featureProject(shareId) {
  if (!confirm('Feature this project on the homepage?')) {return;}
    
  try {
    const response = await fetch(`/admin/api/gallery/${shareId}/feature`, {
      method: 'POST',
      headers: {'Content-Type': 'application/json'}
    });
        
    if (!response.ok) {throw new Error('Failed to feature project');}
        
    await loadProjects();
  } catch (error) {
    console.error('Failed to feature project:', error);
    alert('Failed to feature project. Please try again.');
  }
}

async function unfeatureProject(shareId) {
  if (!confirm('Remove featured status from this project?')) {return;}
    
  try {
    const response = await fetch(`/admin/api/gallery/${shareId}/unfeature`, {
      method: 'POST',
      headers: {'Content-Type': 'application/json'}
    });
        
    if (!response.ok) {throw new Error('Failed to unfeature project');}
        
    await loadProjects();
  } catch (error) {
    console.error('Failed to unfeature project:', error);
    alert('Failed to unfeature project. Please try again.');
  }
}

async function removeProject(shareId) {
  if (!confirm('Permanently remove this project from the gallery? This cannot be undone.')) {return;}
    
  try {
    const response = await fetch(`/admin/api/gallery/${shareId}`, {
      method: 'DELETE',
      headers: {'Content-Type': 'application/json'}
    });
        
    if (!response.ok) {throw new Error('Failed to remove project');}
        
    await loadProjects();
  } catch (error) {
    console.error('Failed to remove project:', error);
    alert('Failed to remove project. Please try again.');
  }
}

// Utility functions
function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}

function formatDate(dateString) {
  if (!dateString) {return 'Unknown';}
  const date = new Date(dateString);
  return date.toLocaleDateString('en-US', {
    year: 'numeric',
    month: 'short',
    day: 'numeric'
  });
}

function formatNumber(num) {
  return num.toLocaleString();
}

// Variable to store all shares
let allShares = [];
let currentShareFilter = 'all';

// Function to show all shares modal
async function showAllShares(filter = 'all') {
  const modal = document.getElementById('shares-modal');
  const modalTitle = document.getElementById('shares-modal-title');
  const sharesList = document.getElementById('shares-list');
  const searchInput = document.getElementById('shares-search-input');
  
  currentShareFilter = filter;
  
  // Update title based on filter
  const titles = {
    'all': 'All Shares',
    'public': 'Public Shares',
    'private': 'Private Shares',
    'with-commands': 'Shares with Commands'
  };
  modalTitle.textContent = titles[filter] || 'All Shares';
  
  // Clear search input
  searchInput.value = '';
  
  // Show modal
  modal.style.display = 'flex';
  sharesList.innerHTML = '<div class="loading">Loading shares...</div>';
  
  try {
    // Fetch all shares if not already loaded
    if (allShares.length === 0) {
      const response = await fetch('/admin/api/all-shares');
      if (!response.ok) {throw new Error('Failed to load shares');}
      const data = await response.json();
      allShares = data.shares || [];
    }
    
    // Render shares with filter
    renderShares(filter);
    
    // Set up search handler
    searchInput.oninput = () => renderShares(filter, searchInput.value);
    
  } catch (error) {
    console.error('Failed to load shares:', error);
    sharesList.innerHTML = '<div class="error">Failed to load shares</div>';
  }
}

// Function to render shares with filter and search
function renderShares(filter = 'all', searchQuery = '') {
  const sharesList = document.getElementById('shares-list');
  const sharesCount = document.getElementById('shares-count');
  
  // Filter shares based on selected filter
  let filteredShares = allShares;
  if (filter === 'public') {
    filteredShares = allShares.filter(s => s.is_public);
  } else if (filter === 'private') {
    filteredShares = allShares.filter(s => !s.is_public);
  } else if (filter === 'with-commands') {
    filteredShares = allShares.filter(s => s.include_commands);
  }
  
  // Apply search filter
  if (searchQuery) {
    const query = searchQuery.toLowerCase();
    filteredShares = filteredShares.filter(share => {
      const projectName = (share.project_name || 'Unknown Project').toLowerCase();
      const shareId = share.id.toLowerCase();
      const date = formatShareDate(share.created_at).toLowerCase();
      return projectName.includes(query) || shareId.includes(query) || date.includes(query);
    });
  }
  
  // Update count
  sharesCount.textContent = `${filteredShares.length} share${filteredShares.length !== 1 ? 's' : ''}`;
  
  // Render shares
  if (filteredShares.length === 0) {
    sharesList.innerHTML = searchQuery ? 
      '<div class="no-results">No shares match your search</div>' :
      '<div class="no-results">No shares found</div>';
  } else {
    sharesList.innerHTML = filteredShares.map(share => `
      <div class="share-item">
        <div class="share-info">
          <div>
            <div class="share-project">${escapeHtml(share.project_name || 'Unknown Project')}</div>
            <div class="share-date">${formatShareDate(share.created_at)}</div>
          </div>
          <div>
            <span class="share-badge ${share.is_public ? 'badge-public' : 'badge-private'}">
              ${share.is_public ? 'Public' : 'Private'}
            </span>
          </div>
          <div>
            ${share.include_commands ? '<span class="share-badge badge-commands">Commands</span>' : ''}
          </div>
          <div>
            <code>${share.id}</code>
          </div>
        </div>
        <div class="share-actions">
          <a href="${share.share_url}" target="_blank" class="btn btn-view">View</a>
          <button class="btn btn-remove" onclick="removeShare('${share.id}')">Delete</button>
        </div>
      </div>
    `).join('');
  }
}

// Function to close the shares modal
function closeSharesModal() {
  document.getElementById('shares-modal').style.display = 'none';
  // Clear the search input
  document.getElementById('shares-search-input').value = '';
  // Reset shares to force reload next time
  allShares = [];
}

// Function to remove a share
async function removeShare(shareId) {
  if (!confirm('Are you sure you want to delete this share? This action cannot be undone.')) {
    return;
  }
  
  try {
    const response = await fetch(`/admin/api/gallery/${shareId}`, {
      method: 'DELETE'
    });
    
    if (!response.ok) {
      const errorData = await response.json().catch(() => ({}));
      throw new Error(errorData.detail || `HTTP ${response.status}`);
    }
    
    const result = await response.json();
    
    // Remove from local array
    allShares = allShares.filter(s => s.id !== shareId);
    
    // Refresh the modal with current filter and search
    const searchInput = document.getElementById('shares-search-input');
    renderShares(currentShareFilter, searchInput.value);
    
    // Reload stats
    await loadShareStats();
    
    // Reload projects if it was in the gallery
    if (result.was_public) {
      await loadProjects();
    }
    
    // Show success message (optional)
    console.log(`Share ${shareId} deleted successfully`);
    
  } catch (error) {
    console.error('Failed to delete share:', error);
    alert(`Failed to delete share: ${error.message}`);
  }
}

// Helper function to format share date
function formatShareDate(dateString) {
  const date = new Date(dateString);
  return date.toLocaleDateString() + ' ' + date.toLocaleTimeString();
}

// Close modal when clicking outside
window.addEventListener('click', (e) => {
  const modal = document.getElementById('shares-modal');
  if (e.target === modal) {
    closeSharesModal();
  }
});