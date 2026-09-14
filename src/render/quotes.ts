// Ambient micro-copy - canonical Taoist / Zen / Buddhist lines, ported verbatim
// from Satori's ui/quotes.ts. One is drawn at random per state. Sources are noted
// in comments but intentionally not shown in the UI.

const POOLS = {
    loading: [
        'Wait quietly while the mud settles.',               // Tao Te Ching 15
        'Like a deep lake, clear and still.',                // Dhammapada 82
        'The ten thousand things arise; watch them return.', // Tao Te Ching 16
    ],
    caughtUp: [
        'Sitting quietly, doing nothing. Spring comes, and the grass grows by itself.', // Zenrin
        'Know when to stop, and you meet no danger.',        // Tao Te Ching 44
        'Contentment is the greatest wealth.',               // Dhammapada 204
        'He who knows he has enough is rich.',               // Tao Te Ching 33
    ],
    empty: [
        'It is the empty space that makes the vessel useful.', // Tao Te Ching 11
        'Empty yourself, and let the mind grow still.',        // Tao Te Ching 16
        'Calm in mind, calm in speech.',                       // Dhammapada 96
    ],
    seek: [
        'The Tao that can be named is not the eternal Tao.',             // Tao Te Ching 1
        'Names are the guest of reality.',                               // Zhuangzi 1 (名者實之賓也)
        'Do not seek the footsteps of the wise; seek what they sought.', // Matsuo Bashō
        'Not knowing is most intimate.',                                 // Zen - Dizang / Book of Serenity
        'Knowing others is wisdom; knowing yourself, enlightenment.',    // Tao Te Ching 33
    ],
};

export type Mood = keyof typeof POOLS;

/** A random real quote fitting the given state. */
export function quote(mood: Mood): string {
    const pool = POOLS[mood];
    return pool[Math.floor(Math.random() * pool.length)]!;
}
