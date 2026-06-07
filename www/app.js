/* ============================================
   UDP2Mic — App.js
   交互逻辑
   ============================================ */

document.addEventListener('DOMContentLoaded', () => {

  /* ── Mobile Nav Toggle ── */
  const toggleBtn = document.querySelector('.nav-toggle');
  const navLinks = document.querySelector('.nav-links');

  if (toggleBtn) {
    toggleBtn.addEventListener('click', () => {
      navLinks.classList.toggle('open');
    });

    // Close nav on link click (mobile)
    document.querySelectorAll('.nav-links a').forEach(link => {
      link.addEventListener('click', () => {
        navLinks.classList.remove('open');
      });
    });
  }

  /* ── Build Tabs ── */
  const tabs = document.querySelectorAll('.build-tab');
  const panels = {
    'build-win': document.getElementById('build-win'),
    'build-android': document.getElementById('build-android'),
  };

  tabs.forEach(tab => {
    tab.addEventListener('click', () => {
      const target = tab.dataset.target;

      // Update tab active state
      tabs.forEach(t => t.classList.remove('active'));
      tab.classList.add('active');

      // Show target panel
      Object.values(panels).forEach(p => {
        if (p) p.classList.remove('active');
      });
      if (panels[target]) {
        panels[target].classList.add('active');
      }
    });
  });

  /* ── Code Copy ── */
  document.querySelectorAll('.code-copy').forEach(btn => {
    btn.addEventListener('click', () => {
      const codeBlock = btn.nextElementSibling;
      const code = codeBlock ? codeBlock.textContent : '';
      const textToCopy = btn.dataset.copy || code.trim();

      navigator.clipboard.writeText(textToCopy).then(() => {
        const original = btn.textContent;
        btn.textContent = '已复制';
        btn.classList.add('copied');

        setTimeout(() => {
          btn.textContent = original;
          btn.classList.remove('copied');
        }, 2000);
      }).catch(() => {
        // Fallback for older browsers
        const textarea = document.createElement('textarea');
        textarea.value = textToCopy;
        textarea.style.position = 'fixed';
        textarea.style.opacity = '0';
        document.body.appendChild(textarea);
        textarea.select();
        document.execCommand('copy');
        document.body.removeChild(textarea);

        const original = btn.textContent;
        btn.textContent = '已复制';
        btn.classList.add('copied');
        setTimeout(() => {
          btn.textContent = original;
          btn.classList.remove('copied');
        }, 2000);
      });
    });
  });

  /* ── Intersection Observer for staggered reveals ── */
  const observer = new IntersectionObserver((entries) => {
    entries.forEach(entry => {
      if (entry.isIntersecting) {
        entry.target.style.opacity = '1';
        entry.target.style.transform = 'translateY(0)';
      }
    });
  }, {
    threshold: 0.1,
    rootMargin: '0px 0px -40px 0px',
  });

  document.querySelectorAll('.feature-card, .step, .flow-node, .spec-card, .download-card').forEach(el => {
    // Preserve any inline animation properties; observer just ensures visibility
    observer.observe(el);
  });

  /* ── Smooth scroll for anchor links (fallback for Safari) ── */
  document.querySelectorAll('a[href^="#"]').forEach(anchor => {
    anchor.addEventListener('click', (e) => {
      const targetId = anchor.getAttribute('href');
      if (targetId === '#') return;

      const target = document.querySelector(targetId);
      if (target) {
        e.preventDefault();
        target.scrollIntoView({ behavior: 'smooth', block: 'start' });
      }
    });
  });
});
