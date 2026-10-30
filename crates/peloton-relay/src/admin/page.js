(function () {
  'use strict';
  document.querySelectorAll('button[data-copy]').forEach(function (button) {
    var source = document.getElementById(button.dataset.copy);
    var label = button.querySelector('.label');
    var idle = label.textContent;
    var timer;
    function say(text) {
      label.textContent = text;
      clearTimeout(timer);
      timer = setTimeout(function () { label.textContent = idle; }, 2000);
    }
    button.hidden = false;
    button.addEventListener('click', function () {
      navigator.clipboard.writeText(source.textContent).then(function () {
        say('Copied');
      }, function () {
        // Selected, so that it can be copied by hand.
        window.getSelection().selectAllChildren(source);
        say('Copy failed');
      });
    });
  });
  var panel = document.querySelector('[data-status]');
  if (!panel) {
    return;
  }
  panel.querySelectorAll('.without-script').forEach(function (element) { element.hidden = true; });
  panel.querySelectorAll('.with-script').forEach(function (element) { element.hidden = false; });
  function finish(id) {
    document.getElementById('renew-steps').hidden = true;
    document.getElementById('renew-note').hidden = true;
    document.getElementById(id).hidden = false;
  }
  function poll() {
    fetch(panel.dataset.status, { cache: 'no-store', credentials: 'same-origin' })
      .then(function (response) {
        if (!response.ok) {
          throw new Error('status ' + response.status);
        }
        return response.json();
      })
      .then(function (status) {
        if (status.code === 'used' && status.health === 'ok') {
          finish('renew-done');
        } else if (status.code === 'used') {
          finish('renew-unusable');
        } else if (status.code === 'refused') {
          finish('renew-refused');
        } else if (status.code === 'expired') {
          finish('renew-expired');
        } else if (status.code === 'none') {
          finish('renew-restarted');
        } else {
          setTimeout(poll, 2000);
        }
      }, function () {
        setTimeout(poll, 5000);
      });
  }
  setTimeout(poll, 2000);
})();
