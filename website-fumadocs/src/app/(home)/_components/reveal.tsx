'use client';

import { useEffect, useRef, useState, type ReactNode } from 'react';

/**
 * Reveals its children with a fade-up the first time they scroll into view.
 * CSS transition only, no dependencies. Respects prefers-reduced-motion and
 * degrades to "always visible" if the observer never fires.
 */
export function Reveal({
  children,
  delay = 0,
  className = '',
}: {
  children: ReactNode;
  delay?: number;
  className?: string;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [shown, setShown] = useState(false);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;

    if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) {
      setShown(true);
      return;
    }

    const io = new IntersectionObserver(
      ([entry]) => {
        if (entry.isIntersecting) {
          setShown(true);
          io.disconnect();
        }
      },
      { threshold: 0.15, rootMargin: '0px 0px -8% 0px' },
    );
    io.observe(el);

    // Safety: never leave content hidden if the observer misbehaves.
    const t = window.setTimeout(() => setShown(true), 1500);
    return () => {
      io.disconnect();
      window.clearTimeout(t);
    };
  }, []);

  return (
    <div
      ref={ref}
      style={{ transitionDelay: shown ? `${delay}ms` : '0ms' }}
      className={`transition-all duration-700 ease-out ${
        shown ? 'translate-y-0 opacity-100' : 'translate-y-4 opacity-0'
      } ${className}`}
    >
      {children}
    </div>
  );
}
