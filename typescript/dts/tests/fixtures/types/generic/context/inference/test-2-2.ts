type Box<BV> = { value: BV };

declare function compose<A, B, C>(f: (a: A) => B, g: (b: B) => C): (a: A) => C;

declare function list<E>(a: E): E[];

declare function box<V>(x: V): Box<V>;

const f11: <T>(x: T) => Box<T[]> = compose(list, box);
