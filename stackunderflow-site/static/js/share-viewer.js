// Share viewer - renders the shared dashboard data
let sharedCommands = []; // Store commands for modal access
let filteredCommands = []; // Store filtered commands
let sharedSortColumn = 'timestamp';
let sharedSortDirection = 'desc';
let sharedCurrentPage = 1;
let sharedCommandsPerPage = 20; // Default, can be changed by user
let sharedTotalPages = 1;

document.addEventListener('DOMContentLoaded', function() {
  if (!window.SHARE_DATA) {
    document.getElementById('dashboard-container').innerHTML = '<div class="error">No share data available</div>';
    return;
  }
    
  const data = window.SHARE_DATA;
  console.log('Share data loaded:', {
    hasStatistics: !!data.statistics,
    statisticsKeys: data.statistics ? Object.keys(data.statistics) : [],
    chartsCount: data.charts ? data.charts.length : 0,
    userCommandsCount: data.user_commands ? data.user_commands.length : 0,
    messagesCount: data.messages ? data.messages.length : 0
  });
  
  // Display project name in header
  if (data.project_name) {
    const projectInfoEl = document.getElementById('project-info-text');
    if (projectInfoEl) {
      projectInfoEl.textContent = data.project_name;
    }
    // Also update the page title
    document.title = `${data.project_name} - Claude Code Analytics`;
  }
    
  // Render statistics
  if (data.statistics && Object.keys(data.statistics).length > 0) {
    renderStatistics(data.statistics);
  }
    
  // Render charts
  if (data.charts && data.charts.length > 0) {
    renderCharts(data.charts);
  }
    
  // Render tables if available
  if (data.user_commands && data.user_commands.length > 0) {
    renderUserCommands(data.user_commands);
  }
    
  if (data.messages && data.messages.length > 0) {
    renderMessages(data.messages);
  }
});

function renderStatistics(stats) {
  if (!stats || !stats.overview) {return;}
  
  // Check if we have the StatsCardsModule available
  if (window.StatsCardsModule && window.StatsCardsModule.displayOverviewStats) {
    // Use the shared module
    window.StatsCardsModule.displayOverviewStats(stats);
  } else {
    // Fallback: render a simple message if module not available
    const container = document.getElementById('overview-stats');
    container.innerHTML = '<div class="error">Stats module not loaded</div>';
  }
}

function renderCharts(chartsData) {
  if (!chartsData || chartsData.length === 0) {return;}
    
  const container = document.querySelector('.charts-section');
    
  chartsData.forEach(chart => {
    const chartContainer = document.createElement('div');
    chartContainer.className = 'chart-container';
    
    // Check if this is the old PNG format or new configuration format
    if (chart.image) {
      // Legacy PNG format
      chartContainer.innerHTML = `
            <h2>${getChartTitle(chart.name)}</h2>
            <img src="data:image/png;base64,${chart.image}" alt="${chart.name}" style="width: 100%; height: auto;">
        `;
    } else if (chart.type && chart.data) {
      // New interactive chart format
      chartContainer.innerHTML = `
            <h2>${getChartTitle(chart.name)}</h2>
            <canvas id="${chart.id || chart.name}"></canvas>
        `;
      container.appendChild(chartContainer);
      
      // Create Chart.js instance
      const canvas = chartContainer.querySelector('canvas');
      const ctx = canvas.getContext('2d');
      
      // Create new chart with the provided configuration
      new Chart(ctx, {
        type: chart.type,
        data: chart.data,
        options: chart.options
      });
      
      return; // Skip the appendChild below since we already did it
    }
    
    container.appendChild(chartContainer);
  });
}

function renderUserCommands(commands) {
  // Store commands globally for modal access
  sharedCommands = commands;
  filteredCommands = [...commands]; // Initialize filtered commands
  
  const container = document.getElementById('dashboard-container');
  const section = document.createElement('div');
  section.className = 'user-commands-section table-section-wide';
  
  // Calculate interruption counts
  const interruptions = commands.filter(cmd => 
    isInterruptionMessage(cmd.user_message)
  ).length;
  const regularCommands = commands.length - interruptions;
  
  // Calculate averages
  const totalSteps = commands.reduce((sum, cmd) => sum + (cmd.assistant_steps || 0), 0);
  const avgSteps = commands.length > 0 ? (totalSteps / commands.length).toFixed(1) : 0;
  const commandsWithTools = commands.filter(cmd => (cmd.tools_used || 0) > 0).length;
  const percentageWithTools = commands.length > 0 ? Math.round((commandsWithTools / commands.length) * 100) : 0;
  
  section.innerHTML = `
        <h2>User Commands (${commands.length})</h2>
        <div class="table-header">
            <div>
                <div id="commands-summary" class="table-summary">
                    <div style="display: flex; flex-direction: column; gap: 0.25rem; width: 100%;">
                        <div style="font-size: 0.9rem; color: #666;">
                            <strong>${commands.length}</strong> total
                            (<strong>${regularCommands}</strong> commands, <strong>${interruptions}</strong> interruptions)
                        </div>
                        <div style="font-size: 0.9rem; color: #666;">
                            ${percentageWithTools}% use tools
                            • Avg ${avgSteps} steps/command
                        </div>
                    </div>
                </div>
            </div>
            <div class="table-controls">
                <div class="filter-group">
                    <input type="text" id="commands-search" placeholder="Search commands..." 
                        oninput="filterSharedCommands()" class="filter-input-search">
                </div>
                <div class="filter-group">
                    <select id="commands-per-page" onchange="updateCommandsPerPage()">
                        <option value="20" selected>20</option>
                        <option value="50">50</option>
                        <option value="100">100</option>
                        <option value="200">200</option>
                    </select>
                </div>
            </div>
        </div>
        <div class="table-wrapper">
            <table class="data-table" id="shared-commands-table">
                <thead>
                    <tr>
                        <th style="width: 35%;" onclick="sortSharedTable('user_message')" class="sortable">User Command</th>
                        <th style="width: 15%;" onclick="sortSharedTable('timestamp')" class="sortable">Timestamp</th>
                        <th style="width: 10%;" onclick="sortSharedTable('model')" class="sortable">Model</th>
                        <th style="width: 8%; text-align: center;" onclick="sortSharedTable('assistant_steps')" class="sortable">Steps</th>
                        <th style="width: 8%; text-align: center;" onclick="sortSharedTable('tools_used')" class="sortable">Tools</th>
                        <th style="width: 8%; text-align: center;" onclick="sortSharedTable('followed_by_interruption')" class="sortable">Interrupted</th>
                        <th style="width: 16%;" class="no-sort">Tool Names</th>
                    </tr>
                </thead>
                <tbody id="shared-commands-table-body">
                    <!-- Commands will be rendered here -->
                </tbody>
            </table>
        </div>
        <div class="table-pagination" id="commands-pagination" style="display: none;">
            <button onclick="changeCommandsPage(-1)" id="commands-prev-btn">Previous</button>
            <div class="page-info">
                Page <input type="number" id="commands-page-input" value="1" onchange="goToCommandsPage()">
                of <span id="commands-total-pages">1</span>
            </div>
            <button onclick="changeCommandsPage(1)" id="commands-next-btn">Next</button>
            <div class="row-jump">
                <label>Go to row:</label>
                <input type="number" id="commands-row-input" 
                    placeholder="Row #" onkeypress="if(event.key==='Enter') goToCommandRow()">
                <button onclick="goToCommandRow()">Go</button>
            </div>
        </div>
    `;
  container.appendChild(section);
  
  // Sort by timestamp descending (newest first) initially
  // Don't use sortSharedTable as it might toggle direction
  // Just sort directly and display
  if (window.sortTableData) {
    const columnConfig = {
      'timestamp': (cmd) => cmd.timestamp || ''
    };
    window.sortTableData(filteredCommands, 'timestamp', 'desc', columnConfig);
  } else {
    // Fallback inline sort
    filteredCommands.sort((a, b) => {
      const aVal = a.timestamp ? new Date(a.timestamp).getTime() : 0;
      const bVal = b.timestamp ? new Date(b.timestamp).getTime() : 0;
      return bVal - aVal; // desc order (newest first)
    });
  }
  displaySharedCommands();
}

function renderMessages(messages) {
  const container = document.getElementById('dashboard-container');
  const section = document.createElement('div');
  section.className = 'messages-section';
  section.innerHTML = `
        <h2>All Messages (${messages.length})</h2>
        <div class="table-container">
            <table>
                <thead>
                    <tr>
                        <th>#</th>
                        <th>Type</th>
                        <th>Model</th>
                        <th>Timestamp</th>
                        <th>Tokens</th>
                    </tr>
                </thead>
                <tbody>
                    ${messages.slice(0, 100).map((msg, i) => `
                        <tr>
                            <td>${i + 1}</td>
                            <td>${msg.type || 'unknown'}</td>
                            <td>${msg.model || '-'}</td>
                            <td>${formatTimestamp(msg.timestamp)}</td>
                            <td>${msg.tokens || 0}</td>
                        </tr>
                    `).join('')}
                </tbody>
            </table>
        </div>
        ${messages.length > 100 ? '<p class="note">Showing first 100 messages</p>' : ''}
    `;
  container.appendChild(section);
}

// Helper functions
function formatNumber(num) {
  if (!num) {return '0';}
  if (num >= 1000000) {return (num / 1000000).toFixed(1) + 'M';}
  if (num >= 1000) {return (num / 1000).toFixed(1) + 'K';}
  return num.toLocaleString();
}

// Note: These helper functions are now provided by the StatsModule and stats-cards.js
// Keep calculateDuration as it's used elsewhere in this file
function calculateDuration(stats) {
  if (!stats.overview) {return 0;}
  const start = new Date(stats.overview.first_message_time);
  const end = new Date(stats.overview.last_message_time);
  return Math.ceil((end - start) / (1000 * 60 * 60 * 24)) + 1;
}

function getChartTitle(name) {
  const titles = {
    'user-interactions': 'User Command Analysis',
    'userInteractionsChart': 'User Command Analysis',
    'command-complexity': 'Command Complexity Over Time',
    'commandComplexityChart': 'Command Complexity Over Time',
    'command-length': 'Command Length Over Time',
    'commandLengthChart': 'Command Length Over Time',
    'tool-usage': 'Tool Usage',
    'toolsChart': 'Tool Usage',
    'tool-trends': 'Tool Usage Trends',
    'toolTrendsChart': 'Tool Usage Trends',
    'error-rate-trend': 'Error Rate Over Time', 
    'errorRateTrendChart': 'Error Rate Over Time',
    'error-distribution': 'Error Type Distribution',
    'errorDistributionChart': 'Error Type Distribution',
    'model-usage': 'Model Usage Distribution',
    'modelUsageChart': 'Model Usage Distribution',
    'token-usage': 'Token Usage',
    'tokensChart': 'Token Usage',
    'daily-cost': 'Daily Cost Breakdown',
    'dailyCostChart': 'Daily Cost Breakdown',
    'hourly-tokens': 'Token Usage by Hour',
    'hourlyTokensChart': 'Token Usage by Hour'
  };
  return titles[name] || name;
}

function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}

function formatTimestamp(timestamp) {
  if (!timestamp) {return '-';}
  const date = new Date(timestamp);
  
  // Show full date and time in local timezone (matching original dashboard format)
  return date.toLocaleString('en-US', {
    year: 'numeric',
    month: 'short',
    day: 'numeric',
    hour: 'numeric',
    minute: '2-digit',
    second: '2-digit',
    hour12: true
  });
}

// Helper function to check if a message is an interruption
function isInterruptionMessage(message) {
  if (!message) {return false;}
  const lowerMsg = message.toLowerCase();
  return lowerMsg.includes('[request interrupted by user') || 
         lowerMsg.includes('[user interrupted]') ||
         lowerMsg.includes('request was interrupted');
}

// Helper function to format model names
function formatModelName(model) {
  if (!model) {return '';}
  // Remove common prefixes
  return model.replace(/^(claude-|anthropic\/)/, '');
}

// Filter shared commands based on search
function filterSharedCommands() {
  const searchTerm = document.getElementById('commands-search').value.toLowerCase();
  
  if (!searchTerm) {
    filteredCommands = [...sharedCommands];
  } else {
    filteredCommands = sharedCommands.filter(cmd => {
      return cmd.user_message.toLowerCase().includes(searchTerm) ||
             (cmd.tool_names && cmd.tool_names.some(tool => tool.toLowerCase().includes(searchTerm)));
    });
  }
  
  sharedCurrentPage = 1; // Reset to first page when filtering
  displaySharedCommands();
}

// Sort shared commands table
function sortSharedTable(column) {
  // Toggle sort direction if clicking the same column
  if (sharedSortColumn === column) {
    sharedSortDirection = sharedSortDirection === 'asc' ? 'desc' : 'asc';
  } else {
    sharedSortColumn = column;
    sharedSortDirection = 'desc'; // Default to descending for new column
  }
  
  // Use sortTableData from utils if available, otherwise implement inline
  if (window.sortTableData) {
    const columnConfig = {
      'user_message': (cmd) => cmd.user_message || '',
      'timestamp': (cmd) => cmd.timestamp || '',
      'model': (cmd) => cmd.model || '',
      'assistant_steps': (cmd) => cmd.assistant_steps || 0,
      'tools_used': (cmd) => cmd.tools_used || 0,
      'followed_by_interruption': (cmd) => cmd.followed_by_interruption ? 1 : 0
    };
    
    window.sortTableData(filteredCommands, sharedSortColumn, sharedSortDirection, columnConfig);
  } else {
    // Fallback inline sort
    filteredCommands.sort((a, b) => {
      let aVal = a[sharedSortColumn] || '';
      let bVal = b[sharedSortColumn] || '';
      
      // For timestamp column, convert to Date objects for proper comparison
      if (sharedSortColumn === 'timestamp') {
        aVal = aVal ? new Date(aVal).getTime() : 0;
        bVal = bVal ? new Date(bVal).getTime() : 0;
      }
      
      if (aVal < bVal) {return sharedSortDirection === 'asc' ? -1 : 1;}
      if (aVal > bVal) {return sharedSortDirection === 'asc' ? 1 : -1;}
      return 0;
    });
  }
  
  sharedCurrentPage = 1; // Reset to first page when sorting
  displaySharedCommands();
}

// Display filtered and sorted commands with pagination
function displaySharedCommands() {
  const tbody = document.getElementById('shared-commands-table-body');
  if (!tbody) {return;}
  
  // Calculate pagination
  sharedTotalPages = Math.ceil(filteredCommands.length / sharedCommandsPerPage);
  
  // Ensure current page is valid
  if (sharedCurrentPage > sharedTotalPages) {sharedCurrentPage = sharedTotalPages;}
  if (sharedCurrentPage < 1) {sharedCurrentPage = 1;}
  
  // Get commands for current page
  const startIndex = (sharedCurrentPage - 1) * sharedCommandsPerPage;
  const endIndex = startIndex + sharedCommandsPerPage;
  const pageCommands = filteredCommands.slice(startIndex, endIndex);
  
  tbody.innerHTML = pageCommands.map((cmd, index) => {
    // Find original index for showCommandDetail
    const originalIndex = sharedCommands.indexOf(cmd);
    
    // Format tool names display
    let toolNamesDisplay = '';
    if (cmd.tool_names && cmd.tool_names.length > 0) {
      // Count occurrences of each tool
      const toolCounts = {};
      cmd.tool_names.forEach(tool => {
        // Handle both string and object cases
        const toolName = typeof tool === 'string' ? tool : (tool.name || 'Unknown');
        toolCounts[toolName] = (toolCounts[toolName] || 0) + 1;
      });
      
      // Get unique tools in order of first appearance
      const uniqueTools = [];
      const seen = new Set();
      cmd.tool_names.forEach(tool => {
        const toolName = typeof tool === 'string' ? tool : (tool.name || 'Unknown');
        if (!seen.has(toolName)) {
          seen.add(toolName);
          uniqueTools.push(toolName);
        }
      });
      
      if (uniqueTools.length <= 3) {
        // Show all tools with counts if repeated
        toolNamesDisplay = uniqueTools.map(tool => {
          const count = toolCounts[tool];
          return count > 1 ? `${tool}(${count})` : tool;
        }).join(', ');
      } else {
        // Show first 3 tools with counts and "+N more"
        const firstThree = uniqueTools.slice(0, 3).map(tool => {
          const count = toolCounts[tool];
          return count > 1 ? `${tool}(${count})` : tool;
        }).join(', ');
        const moreCount = uniqueTools.length - 3;
        toolNamesDisplay = `${firstThree}, <span style="color: #667eea;">+${moreCount} more</span>`;
      }
    } else {
      toolNamesDisplay = '<span style="color: #999;">-</span>';
    }
    
    return `
      <tr onclick="showCommandDetail(${originalIndex})" style="cursor: pointer;">
          <td style="overflow: hidden; max-width: 300px;">
              <div style="overflow: hidden; text-overflow: ellipsis; white-space: nowrap; word-break: break-all;" 
                   title="${escapeHtml(cmd.user_message)}">
                  ${escapeHtml(cmd.user_message_truncated || cmd.user_message)}
              </div>
          </td>
          <td style="font-size: 0.9rem;">${formatTimestamp(cmd.timestamp)}</td>
          <td style="font-size: 0.85rem;">
              ${cmd.model && cmd.model !== 'N/A' ? 
    formatModelName(cmd.model) : 
    '<span style="color: #999;">-</span>'}
          </td>
          <td style="text-align: center;">${cmd.assistant_steps || 0}</td>
          <td style="text-align: center;">
              ${cmd.tools_used > 0 ? 
    `<span style="color: #667eea; font-weight: bold;">${cmd.tools_used}</span>` : 
    '<span style="color: #999;">0</span>'}
          </td>
          <td style="text-align: center;">
              ${cmd.followed_by_interruption ? 
    '<span style="color: #d32f2f; font-weight: bold;" title="This command was followed by a user interruption">Yes</span>' : 
    '<span style="color: #999;">-</span>'}
          </td>
          <td style="font-size: 0.85rem;">
              ${toolNamesDisplay}
          </td>
      </tr>
    `;
  }).join('');
  
  // Update summary with filtered count
  const summaryEl = document.getElementById('commands-summary');
  if (summaryEl) {
    // Find or create the filter status element
    let filterStatus = summaryEl.querySelector('.filter-status');
    if (!filterStatus) {
      filterStatus = document.createElement('div');
      filterStatus.className = 'filter-status';
      filterStatus.style.cssText = 'font-size: 0.85rem; color: #888; margin-top: 0.25rem;';
      summaryEl.appendChild(filterStatus);
    }
    
    // Update or hide the filter status
    if (filteredCommands.length !== sharedCommands.length) {
      filterStatus.textContent = `Showing ${filteredCommands.length} of ${sharedCommands.length} commands`;
      filterStatus.style.display = 'block';
    } else {
      filterStatus.style.display = 'none';
    }
  }
  
  // Update pagination controls
  updatePaginationControls();
}

// Show detailed view of a command
function showCommandDetail(index) {
  const cmd = sharedCommands[index];
  if (!cmd) {return;}
  
  // Count tool occurrences
  const toolCounts = {};
  if (cmd.tool_names && cmd.tool_names.length > 0) {
    cmd.tool_names.forEach(tool => {
      toolCounts[tool] = (toolCounts[tool] || 0) + 1;
    });
  }
  
  // Create formatted list
  const toolsList = Object.entries(toolCounts)
    .sort((a, b) => b[1] - a[1]) // Sort by count descending
    .map(([tool, count]) => {
      const countText = count > 1 ? `<span style="color: #667eea; font-weight: bold;">(${count}x)</span>` : '';
      return `<div style="padding: 0.5rem 0; border-bottom: 1px solid #eee;">
                <span style="font-size: 1rem;">${tool}</span> ${countText}
              </div>`;
    })
    .join('');
  
  // Use the existing modal
  const modal = document.getElementById('message-modal');
  const modalTitle = document.getElementById('modal-title');
  const modalBody = document.getElementById('modal-body');
  
  modalTitle.textContent = 'User Command Details';
  
  // Hide navigation for commands in share viewer
  const modalNav = document.querySelector('.modal-navigation');
  if (modalNav) {
    modalNav.style.display = 'none';
  }
  
  // Trim each line while preserving line breaks
  const trimmedMessage = cmd.user_message
    .split('\n')
    .map(line => line.trim())
    .join('\n')
    .trim();
  
  modalBody.innerHTML = `
    <div style="margin-bottom: 1rem;">
      <div class="detail-content" style="margin-top: 0.5rem; white-space: pre-wrap; word-wrap: break-word;">
        ${escapeHtml(trimmedMessage)}
      </div>
    </div>
    <div style="margin-bottom: 1rem;">
      <strong>Timestamp:</strong> ${formatTimestamp(cmd.timestamp)}
    </div>
    <div style="margin-bottom: 1rem;">
      <strong>Assistant Steps:</strong> ${cmd.assistant_steps}
    </div>
    <div style="margin-bottom: 1rem;">
      <strong>Total Tool Calls:</strong> ${cmd.tools_used}
    </div>
    ${cmd.tools_used > 0 ? `
      <div style="margin-bottom: 1rem;">
        <strong>Tools Used:</strong>
      </div>
      <div style="max-height: 400px; overflow-y: auto;">
        ${toolsList}
      </div>
    ` : ''}
  `;
  
  modal.style.display = 'block';
}

// Update pagination controls visibility and state
function updatePaginationControls() {
  const paginationEl = document.getElementById('commands-pagination');
  const totalPagesEl = document.getElementById('commands-total-pages');
  const pageInputEl = document.getElementById('commands-page-input');
  const prevBtn = document.getElementById('commands-prev-btn');
  const nextBtn = document.getElementById('commands-next-btn');
  
  if (filteredCommands.length > sharedCommandsPerPage) {
    paginationEl.style.display = 'flex';
    totalPagesEl.textContent = sharedTotalPages;
    pageInputEl.value = sharedCurrentPage;
    
    // Update button states
    prevBtn.disabled = sharedCurrentPage === 1;
    nextBtn.disabled = sharedCurrentPage === sharedTotalPages;
  } else {
    paginationEl.style.display = 'none';
  }
}

// Adapter functions to work with shared view state
function changeCommandsPage(delta) {
  const newPage = sharedCurrentPage + delta;
  if (newPage >= 1 && newPage <= sharedTotalPages) {
    sharedCurrentPage = newPage;
    displaySharedCommands();
  }
}

function goToCommandsPage() {
  const pageInput = document.getElementById('commands-page-input');
  const page = parseInt(pageInput.value);
  
  if (!isNaN(page) && page >= 1 && page <= sharedTotalPages) {
    sharedCurrentPage = page;
    displaySharedCommands();
  } else {
    pageInput.value = sharedCurrentPage;
  }
}

function updateCommandsPerPage() {
  const select = document.getElementById('commands-per-page');
  sharedCommandsPerPage = parseInt(select.value);
  sharedCurrentPage = 1;
  displaySharedCommands();
}

function goToCommandRow() {
  // Reuse utility function if available
  if (window.goToRowInPaginatedTable) {
    window.goToRowInPaginatedTable({
      inputId: 'commands-row-input',
      dataArray: filteredCommands,
      itemsPerPage: sharedCommandsPerPage,
      navigateFunction: (page) => {
        sharedCurrentPage = page;
        displaySharedCommands();
      },
      tbodySelector: '#shared-commands-table-body'
    });
  }
}

// Make functions globally accessible for onclick handlers
window.filterSharedCommands = filterSharedCommands;
window.sortSharedTable = sortSharedTable;
window.showCommandDetail = showCommandDetail;
window.changeCommandsPage = changeCommandsPage;
window.goToCommandsPage = goToCommandsPage;
window.updateCommandsPerPage = updateCommandsPerPage;
window.goToCommandRow = goToCommandRow;